// -*- mode: Bluespec; -*-
//
// C-prime calibration override, the B2-strong / GC-after-vouch shape
// (a) flip. Transfer row: §9.3 "the expect-violation check
// probe-vouched-closure-gone is replaced by a holds check + calibration
// pin 'executeStep without pin INSERT' (must re-find the GC trace
// shape)".
//
// PRE-FIX behavior modeled: the executor ingests WITHOUT the
// pin-at-ingest INSERT — the path sits unpinned between ingest and
// consumption, and store GC collects it (the predecessor campaign's
// GC-after-vouch window, closed by §5.1 pin-at-ingest).
//
// PREDICTED: pinCoversIngestUntilAllInterestTerminal falsifies — an
// ingested path of an unresolved job is absent (collected).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibB2NoPinAtIngest \
//     --step=calibStep --invariant=pinCoversIngestUntilAllInterestTerminal \
//     docs/spec/models/calibration/mat-b2-no-pin-at-ingest.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibB2NoPinAtIngest \
//     --invariant=pinCoversIngestUntilAllInterestTerminal \
//     docs/spec/models/calibration/mat-b2-no-pin-at-ingest.qnt
module matCalibB2NoPinAtIngest {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 4,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
    MAX_LEGACY = 0,
    ENABLE_FAILOVER = false,
    ENABLE_GC = true,
    ENABLE_CRASH = false,
    ENABLE_STALE_TENURE = false,
    ENABLE_RESOLVE_FAULTS = false,
    TENANTS = Set("tA", "tB"),
    ENABLE_WALK_FOLD = false,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX ingest: no pin INSERT.
  action calibExecuteStepNoPin(d: str, o: str): bool = all {
    OUTPUTS.contains(o),
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    liveWanted(d).contains(o),
    not(present.get((d, o))),
    upstreamAvail.get((d, o)),
    present' = present.put((d, o), true),
    pins' = pins,
    ingested' = ingested.put((d, o), true),
    jobCoreUnchanged, relUnchanged, ledgerUnchanged, envUnchanged,
    tenureUnchanged, ghostsUnchanged, a4Unchanged,
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
        calibExecuteStepNoPin(d, o),
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
