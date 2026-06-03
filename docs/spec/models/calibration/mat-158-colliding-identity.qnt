// -*- mode: Bluespec; -*-
//
// Bughunt-wave B1 calibration override, the colliding-identity shape
// (merged_bug_158's pre-fix admission): re-delivery on the
// (Materialization, Claimed{held_by_puller}) cell keyed on composite
// identity ALONE — two workers presenting the same identity (the
// pre-B2 `{pod}` template collision, or any HMAC-identity reuse) both
// held the same open materialization attempt: the kernel answered
// DeliverExisting to the second poller because identity agreement was
// the only gate. The production fix is the resume token: re-delivery
// requires the original exec_id (rio-evidence-kernel
// check_materialization_redelivery_requires_resume_token; the T-0e.6
// rule-4 amendment, PENDING owner counter-signature).
//
// PRE-FIX behavior modeled: calibCollidingRedeliver admits a second
// poller onto the open attempt on identity match alone — after the
// step the attempt's holder identity no longer names a unique
// registered replica (two pods behind one name), modeled as the
// holder leaving the REPLICAS universe.
//
// PREDICTED: atMostOneClaimWinner falsifies (the §2.2 arbiter's
// worker-class identity clause).
//
// Run (falsification):
//   quint verify --backend=tlc --main=matCalibCollidingIdentity \
//     --step=calibStep --invariant=atMostOneClaimWinner \
//     docs/spec/models/calibration/mat-158-colliding-identity.qnt
// Baseline (the as-shipped step, must HOLD):
//   quint verify --backend=tlc --main=matCalibCollidingIdentity \
//     --step=step --invariant=atMostOneClaimWinner \
//     docs/spec/models/calibration/mat-158-colliding-identity.qnt
module matCalibCollidingIdentity {
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
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX re-delivery: a second poller with the colliding identity
  /// is DELIVERED the already-open attempt (token-blind
  /// DeliverExisting). The attempt survives with a holder identity
  /// that no longer names a unique registered replica; the view still
  /// believes the original holder — the exact two-workers-one-attempt
  /// skew of the recorded red.
  action calibCollidingRedeliver(d: str): bool = all {
    match attempts.get(d) {
      | NoAttempt => false
      | OpenAttempt(a) => a.kind == KMaterialization
    },
    attempts' = attempts.put(d, match attempts.get(d) {
      | NoAttempt => NoAttempt
      | OpenAttempt(a) => OpenAttempt({ ...a, instance: "collided" })
    }),
    jobs' = jobs, dupJob' = dupJob, view' = view,
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
      dedupRefeed(d),
      dedupPrunedUpgrade(d, b),
      claimJob(d, replica),
      calibCollidingRedeliver(d),
      executeStep(d, o),
      consumeSuccess(d),
      consumeUnobtainable(d, missing),
      reportInfra(d),
      establishCrashedAttempt(d),
      cancelOnZeroInterest(d),
      resolveApplied(d),
      resolveFenced(d),
      resolveErr(d),
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
