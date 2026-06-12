// -*- mode: Bluespec; -*-
//
// Bughunt-wave A3 calibration, the bug_385 head-of-line-starvation
// shape — a DETERMINISTIC RUN PIN, not an invariant falsification:
// pre-fix, poll_and_claim listed LIMIT-1 oldest-first, so a refused
// head (a parked job answers NotYetReady; a Gone head likewise) hid
// every younger claimable job — the pass ended with zero claims,
// forever. The production fix decouples the listing window from the
// claim budget (max(16, slots*8)) with in-pass refusal skip.
//
// Encoding note: the natural sim
// encoding "younger claimed while older pending" is EDGE-shaped — the
// pending head can legitimately appear AFTER a younger claim opened,
// so the state form is reachable under any claim ordering and cannot
// flip. The deterministic run below is the faithful pin: it builds
// the parked-head state, asserts the LIMIT-1 oldest-first gate
// (d2ClaimAdmitted, the pre-fix listing's admission predicate)
// REFUSES the younger claim, then takes the as-built free claim —
// the in-pass skip — from the very same state. The model has no
// creation order; lexicographic d1-before-d2 is the deterministic
// stand-in, and the parked head is the model's image of the work
// order's refused (view-absent / NotYetReady) head.
//
// Run:
//   quint test --main=matCalibHeadStarvation \
//     --match='headStarvationRun' \
//     docs/spec/models/calibration/mat-head-starvation.qnt
module matCalibHeadStarvation {
  import materializationJob(
    BUILDS = Set("b1", "b2"),
    DRVS = Set("d1", "d2"),
    OUTPUTS = Set("o1", "o2"),
    REPLICAS = Set("r1", "r2"),
    BUILDERS = Set("w1"),
    MAT_BUDGET = 2,
    MAX_EXECS = 6,
    MAX_GEN = 0,
    MAX_UPSTREAM_FLIPS = 4,
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

  /// The PRE-FIX LIMIT-1 oldest-first admission predicate for a claim
  /// on the younger drv: admitted only when the older head holds no
  /// pending job. A parked head is pending — it blocks the pass.
  val d2ClaimAdmitted = match jobs.get("d1") {
    | NoJob => true
    | SomeJob(j) => j.state != JPending
  }

  run headStarvationRun = {
    init
      // the head: d1's job parks at the budget (two infra charges).
      .then(all { recordWantedRelation("b1", "d1", Set("o1")), walkUnchanged })
      .then(all { createJob("d1", OProbe, "b1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .then(all { claimJob("d1", "r1"), walkUnchanged })
      .then(all { reportInfra("d1"), walkUnchanged })
      .expect(match jobs.get("d1") {
        | NoJob => false
        | SomeJob(j) => j.state == JPending and j.parked
      })
      // the younger claimable job behind it.
      .then(all { recordWantedRelation("b2", "d2", Set("o2")), walkUnchanged })
      .then(all { createJob("d2", OProbe, "b2"), walkUnchanged })
      .expect(and {
        match jobs.get("d2") {
          | NoJob => false
          | SomeJob(j) => j.state == JPending and not(j.parked)
        },
        // THE STARVATION: the pre-fix LIMIT-1 oldest-first gate
        // refuses the younger claim while the parked head pends.
        not(d2ClaimAdmitted),
      })
      // THE FIX: the as-built claim (listing window decoupled,
      // in-pass refusal skip) claims d2 from the SAME state.
      .then(all { claimJob("d2", "r2"), walkUnchanged })
      .expect(match attempts.get("d2") {
        | NoAttempt => false
        | OpenAttempt(a) => a.kind == KMaterialization
      })
  }
}
