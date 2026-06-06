// -*- mode: Bluespec; -*-
//
// Bughunt-wave A4 calibration override, the merged_bug_193 shape — THE
// REMOVED AS-BUILT ENCODING: the Unobtainable report had ONE missing
// set (`missing ⊆ OUTPUTS`), so a confirmed reference-closure miss was
// UNREPRESENTABLE as itself. The executor folded reference holes away
// (or smeared them into the wanted cell), and the moot arm completed
// nodes whose closure was provably holed — the closure-completeness
// laundering the 4-cell split exists to prevent.
//
// PRE-FIX behavior modeled: calibConsumeRefsFolded is
// consumeUnobtainable with the refs axis IGNORED at the moot arm —
// `arm0covered = arm0 and liveWantedCovered(d)` (no refsMissing
// conjunct) — while the closure-completeness latch records the
// completion-over-a-hole.
//
// PREDICTED: closureCompleteResolution falsifies — a node completed
// while the report carried a confirmed reference hole.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibRefsFolded \
//     --step=calibStep --invariant=closureCompleteResolution \
//     docs/spec/models/calibration/mat-a4-refs-folded.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibRefsFolded \
//     --invariant=closureCompleteResolution \
//     docs/spec/models/calibration/mat-a4-refs-folded.qnt
module matCalibRefsFolded {
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

  /// PRE-FIX moot arm: refsMissing ignored — arm0covered completes
  /// over a confirmed reference hole. Only the moot arm is modeled
  /// (the override targets exactly the laundering edge; the deeper
  /// arms route the same as-built).
  action calibConsumeRefsFolded(d: str, missing: Set[str], refsMissing: bool): bool = all {
    or { missing != Set(), refsMissing },
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
    // THE BUG: the moot arm reads only the wanted cell.
    val arm0 = missingLive == Set()
    val arm0covered = arm0 and liveWantedCovered(d)
    all {
      // Constrain the override to the moot arm (deeper arms keep the
      // as-built consumption — outside this action's space).
      arm0,
      jobs' = jobs.put(d, match jobs.get(d) {
        | NoJob => NoJob
        | SomeJob(j) => SomeJob({ ...j,
            state: if (arm0covered) JResolvedSuccess else JPending,
            resolutionExec: if (arm0covered) execId else j.resolutionExec,
            matUnobtainableCount: j.matUnobtainableCount + 1 })
      }),
      view' = if (arm0covered) view.put(d, VNone) else view.put(d, VPending(false)),
      nodeStatus' = if (arm0covered) nodeStatus.put(d, NCompleted) else nodeStatus.put(d, NQueued),
      attempts' = attempts.put(d, NoAttempt),
      topdownPruned' = if (arm0covered) topdownPruned.put(d, false) else topdownPruned,
      matUnobtainableLedger' = matUnobtainableLedger.put(d, matUnobtainableLedger.get(d) + 1),
      // The latch records the laundering: completion with the hole on
      // the table.
      closureCompleteAll' = and {
        closureCompleteAll,
        arm0covered implies not(refsMissing),
      },
    },
    deferUntil' = deferUntil,
    transientChargeFreeAll' = transientChargeFreeAll,
    noVacuousCoverageAll' = noVacuousCoverageAll,
    everContrib' = everContrib,
    leafReevalReleased' = leafReevalReleased,
    dupJob' = dupJob, execsMinted' = execsMinted,
    relUnchanged, storeUnchanged, envUnchanged, tenureUnchanged,
    matInfraLedger' = matInfraLedger, buildCrashLedger' = buildCrashLedger,
    establishCount' = establishCount, buildCrashEstablished' = buildCrashEstablished,
    unobChargeBacked' = unobChargeBacked,
    ghostsUnchanged,
  }

  action calibStep = {
    nondet d = DRVS.oneOf()
    nondet b = BUILDS.oneOf()
    nondet o = OUTPUTS.oneOf()
    nondet replica = REPLICAS.oneOf()
    nondet outs = OUTPUTS.powerset().oneOf()
    nondet missing = OUTPUTS.powerset().oneOf()
    nondet refsMissing = Set(true, false).oneOf()
    nondet origin = Set(OProbe, OMerge, OPruned, OReprobe, OStaleReset).oneOf()
    all {
      any {
        recordWantedRelation(b, d, outs),
        createJob(d, origin, b),
        claimJob(d, replica),
        executeStep(d, o),
        consumeSuccess(d),
        // the override REPLACES consumeUnobtainable's moot arm
        calibConsumeRefsFolded(d, missing, refsMissing),
        cancelOnZeroInterest(d),
        buildTerminal(b),
        upstreamChange(d, o),
      },
      walkUnchanged,
    }
  }
}
