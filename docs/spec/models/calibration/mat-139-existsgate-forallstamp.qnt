// -*- mode: Bluespec; -*-
//
// Bughunt-3 S3 stamp-law calibration, the bug_139 ∃-gate/∀-stamp
// (HIGH, signed Q2): the walk gated on the FIRST tenant that could
// see a path (probe_local broke at the first visible tenant; the
// substitute loop broke at the first serving tenant) while Success
// consumption stamped path_tenants ownership for EVERY attributed
// tenant — one tenant's verification laundered into all-tenant
// durable ownership. The production fix carries the verified-tenant
// sets on the wire (MaterializationOutcome.Success, R10) and derives
// the stamped pairs FROM the StampProvenance witness
// (live_pins.rs lawful_pairs — THE one body); this calibration
// re-derives the pre-fix cartesian through the SAME stampApply seat
// (P5: perturb the decision, never the latch).
//
// PREDICTED: stampedOnlyVerifiedTenants falsifies on the
// discriminating evidence (verified = {(tA,o1)} under attributed
// {tA,tB} × outputs {o1,o2} — the cartesian exceeds the lawful
// singleton by three pairs).
//
// Run (falsification):
//   quint run --main=matCalibExistsgateForallstamp --init=init --step=calibStep \
//     --invariant=stampedOnlyVerifiedTenants --max-samples=400000 --max-steps=12 \
//     docs/spec/models/calibration/mat-139-existsgate-forallstamp.qnt
// Baseline (the lawful stamp, must HOLD):
//   quint run --main=matCalibExistsgateForallstamp --init=init --step=step \
//     --invariant=stampedOnlyVerifiedTenants --max-samples=100000 --max-steps=12 \
//     docs/spec/models/calibration/mat-139-existsgate-forallstamp.qnt
module matCalibExistsgateForallstamp {
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
    ENABLE_WALK_FOLD = true,
    ENABLE_WORKER_ABORTED = false
  ).* from "../materializationJob"

  /// PRE-FIX stamp: the SAME armed evidence, the pair set derived the
  /// pre-fix way — Success reached because SOME tenant verified
  /// (∃-gate: verified non-empty), so EVERY attributed tenant is
  /// stamped over every output (∀-stamp: the cartesian) — routed
  /// through the live seat (P5).
  action calibForallStamp = all {
    stampApply(
      if (stampEv.verified != Set())
        tuples(stampEv.attributed, stampEv.outputs)
      else Set()
    ),
  }

  action calibStep = any {
    step,
    all { calibForallStamp, machineFrame },
  }
}
