// -*- mode: Bluespec; -*-
//
// C-prime calibration override, Phase-B bug B3 (ce17c6445, finding 18
// gap 1 / delta 2a; store.materialize.executor+2): the executor
// transport pinned to a standby — UNAVAILABLE answers never broke the
// connection, so claims/reports dead-ended forever after a scheduler
// rollout (a deployment-level settlement-totality hole).
//
// PRE-FIX behavior modeled: redialChannel is REMOVED from the alphabet
// — a staled channel stays staled.
//
// PREDICTED — **witness flip, not an invariant falsification** (the
// dead-end is a liveness shape): under calibStep the
// noPostFailoverClaim witness must NOT be violated (no claim can ever
// land after a failover stales the channels — the B3 dead-end made
// visible as unreachability), while under the as-built step the same
// witness IS violated (the wired
// quint-materialization-witness-post-failover-claim). The §9.1 safety
// conjunction stays green under the override — recorded, not a
// falsification target.
//
// Run (expected [ok] — the witness must NOT violate under calibStep):
//   quint verify --backend=tlc --main=matCalibB3NoRedial \
//     --step=calibStep --invariant=noPostFailoverClaim \
//     docs/spec/models/calibration/mat-b3-no-redial.qnt
// As-built direction (must VIOLATE — the wired witness check):
//   quint verify --backend=tlc --main=matCalibB3NoRedial \
//     --invariant=noPostFailoverClaim \
//     docs/spec/models/calibration/mat-b3-no-redial.qnt
module matCalibB3NoRedial {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 2,
    MAX_UPSTREAM_FLIPS = 2,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = true,
    ENABLE_GC = false,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

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
    all {
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
        failover,
        staleTenureWriteDiscarded(d),
        releasePins(d),
        storeGc(d, o),
        upstreamChange(d, o),
        durableRelationChange(d, rel),
      },
      walkUnchanged,
    }
  }
}
