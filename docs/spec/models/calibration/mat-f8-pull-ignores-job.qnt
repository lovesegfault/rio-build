// -*- mode: Bluespec; -*-
//
// C-prime calibration override, families F8 (CE-33 -> A1, the anchor)
// and F13 (CE-58 -> B8 — same falsification, different historical
// action; design §9.3 "same check as F8"). Transfer row: §9.3
// "successor falsification: override 'admit_pull ignores job state'
// falsifies noFromSourceWhileJobUnresolved".
//
// PRE-FIX behavior modeled: the builder-pull admission consults
// neither the materialization-job view nor the A11 must_substitute
// judgment — a Ready node is delivered from source while its job is
// unresolved (the evidence-blind admission CE-33 exhibited, and the
// never-probed dispatch CE-58 exhibited).
//
// PREDICTED: noFromSourceWhileJobUnresolved falsifies — a build-kind
// attempt opens while the PG job row is Pending/Claimed.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibF8PullIgnoresJob \
//     --step=calibStep --invariant=noFromSourceWhileJobUnresolved \
//     docs/spec/models/calibration/mat-f8-pull-ignores-job.qnt
// Baseline (as-built step at the same constants, must HOLD):
//   quint verify --backend=tlc --main=matCalibF8PullIgnoresJob \
//     --invariant=noFromSourceWhileJobUnresolved \
//     docs/spec/models/calibration/mat-f8-pull-ignores-job.qnt
module matCalibF8PullIgnoresJob {
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
    ENABLE_STALE_TENURE = false
  ).* from "../materializationJob"

  /// PRE-FIX builder pull: no view gate, no must_substitute gate. The
  /// node only needs to be Ready with no open attempt.
  action calibBuilderPullNoJobGate(d: str, builder: str): bool = all {
    BUILDERS.contains(builder),
    execsMinted < MAX_EXECS,
    nodeStatus.get(d) == NReady,
    attempts.get(d) == NoAttempt,
    val newExec = execsMinted + 1
    all {
      attempts' = attempts.put(d, OpenAttempt({ kind: KBuild, instance: builder, execId: newExec })),
      nodeStatus' = nodeStatus.put(d, NRunning),
      execsMinted' = newExec,
    },
    jobs' = jobs, dupJob' = dupJob, view' = view, topdownPruned' = topdownPruned,
    relUnchanged, storeUnchanged, ledgerUnchanged, envUnchanged,
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
      createJob(d, origin, b),
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
      calibBuilderPullNoJobGate(d, builder),
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
