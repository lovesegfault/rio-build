// -*- mode: Bluespec; -*-
//
// Bughunt-wave calibration override, the bug_233 shape (D1
// store-gc-claim-state, migration 093).
//
// PRE-FIX behavior modeled: the store client swallowed a job_id parse
// failure into NULL (`Uuid::parse_str(..).ok()`) and claimed anyway —
// the execution then pinned at ingest WITHOUT job attribution. The
// §5.3 release rule resolves pins through their job row, so the
// unattributed pin was IMMORTAL (released by nothing).
//
// PREDICTED: materializationPinHasJob falsifies — a materialization
// pin exists for a derivation with no job row.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalib233UnattributedPin \
//     --step=calibStep --invariant=materializationPinHasJob \
//     docs/spec/models/calibration/mat-233-unattributed-pin.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalib233UnattributedPin \
//     --invariant=materializationPinHasJob \
//     docs/spec/models/calibration/mat-233-unattributed-pin.qnt
module matCalib233UnattributedPin {
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

  /// PRE-FIX ingest: a pin write with NO job attribution (the
  /// NULL-job row the 093 CHECK forbids). The job-row guard the
  /// as-built executeStep carries (an open materialization attempt
  /// implies a claimed job) is exactly what the swallowed parse
  /// bypassed.
  action calibUnattributedPin(d: str, o: str): bool = all {
    OUTPUTS.contains(o),
    jobs.get(d) == NoJob,
    not(present.get((d, o))),
    present' = present.put((d, o), true),
    pins' = pins.put((d, o), PinMaterialization),
    ingested' = ingested.put((d, o), true),
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged,
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
    all {
      any {
        recordWantedRelation(b, d, outs),
        legacyBuildArrives(b, d),
        createJob(d, origin, b),
        dedupRefeed(d),
        claimJob(d, replica),
        executeStep(d, o),
        calibUnattributedPin(d, o),
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
        redialChannel(replica),
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
