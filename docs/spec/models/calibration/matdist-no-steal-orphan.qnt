// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-6b calibration twin, the orphaned remainder:
// rendezvous partition WITHOUT the steal horizon (STEAL_ENABLED =
// false — the "partition only" half-fix). A worker that crashes
// after listing keeps OWNING its slice (the scheduler sees contacts,
// not deaths) and nobody else is served it: the slice strands for
// the whole (STEAL_HORIZON, MEMBER_TTL] gap until the TTL re-keys
// the partition. Const lane of the live module; no action overrides.
//
// PREDICTED: noJobUnlistedForever falsifies (dead owner past the
// horizon, inside the TTL — the job is in no live member's serve
// set and the owner grace is spent) and stealRecoversOrphanedRemainder
// falsifies (the stale-owned job reaches nobody, let alone
// everybody).
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibNoStealOrphan \
//     --invariant=noJobUnlistedForever \
//     docs/spec/models/calibration/matdist-no-steal-orphan.qnt
//   quint run --backend=rust --main=matDistCalibNoStealOrphan \
//     --invariant=stealRecoversOrphanedRemainder \
//     docs/spec/models/calibration/matdist-no-steal-orphan.qnt
module matDistCalibNoStealOrphan {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = false,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false
  ).* from "../materializationDistribution"
}
