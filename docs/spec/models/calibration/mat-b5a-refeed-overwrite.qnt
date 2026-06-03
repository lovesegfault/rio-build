// -*- mode: Bluespec; -*-
//
// C-prime calibration override, Phase-B bug B5(a) (4e57180fd, T-4.3):
// the dispatch-probe dedup re-feed OVERWROTE existing view entries with
// a fresh unparked/unclaimed entry — one probe pass wiped a parked
// job's in-memory park state (premature re-claim eligibility) or a
// claim holder.
//
// PRE-FIX behavior modeled: dedupRefeed writes a fresh
// VPending(false) entry unconditionally (no or_insert).
//
// PREDICTED: unresolvedJobAlwaysArmed falsifies — a parked (or
// claimed) job whose view entry says claimable-unparked.
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibB5aRefeedOverwrite \
//     --step=calibStep --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-b5a-refeed-overwrite.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=matCalibB5aRefeedOverwrite \
//     --invariant=unresolvedJobAlwaysArmed \
//     docs/spec/models/calibration/mat-b5a-refeed-overwrite.qnt
module matCalibB5aRefeedOverwrite {
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

  /// PRE-FIX dedup re-feed: overwrites the view entry fresh.
  action calibDedupRefeedOverwrites(d: str): bool = all {
    hasUnresolvedJob(d),
    view' = view.put(d, VPending(false)),
    jobs' = jobs, dupJob' = dupJob, attempts' = attempts,
    nodeStatus' = nodeStatus, topdownPruned' = topdownPruned,
    execsMinted' = execsMinted,
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
      calibDedupRefeedOverwrites(d),
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
