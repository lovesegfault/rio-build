// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-6b calibration twin, the no-horizon greedy steal:
// every caller is served the WHOLE remainder regardless of owner
// freshness (EAGER_STEAL = true — the client-side/unconditional-
// remainder design RULED CF-3 rejected: unbounded duplication, the
// convoy re-created one hop later). Const lane of the live module;
// no action overrides.
//
// PREDICTED: stealOnlyAfterOwnerStaleness falsifies — the
// prematureSteal ghost latches the first time a beat-list serves a
// non-own job whose owner is FRESH (two fresh members + one
// claimable job suffice).
//
// (P1 twin-per-leaf: the book's two named twins cover the other four
// holds; this third twin completes the falsify pairing for the
// staleness-bound itself — recorded as an R5 delta in the wave log.)
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibEagerSteal \
//     --invariant=stealOnlyAfterOwnerStaleness \
//     docs/spec/models/calibration/matdist-eager-steal.qnt
module matDistCalibEagerSteal {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = true
  ).* from "../materializationDistribution"
}
