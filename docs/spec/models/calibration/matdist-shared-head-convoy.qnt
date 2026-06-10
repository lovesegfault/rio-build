// -*- mode: Bluespec; -*-
//
// Bughunt-5 slot-6b calibration twin, live_041's as-built world: NO
// owner law — every store worker polls (and "owns") the SAME
// deterministic `ORDER BY created_at` head. The degenerate owner map
// (ownersOf = the whole membership) is the SHARED_HEAD_CONVOY const
// lane of the live module; no action overrides ((cc)/(ccccc): all
// consts bound, default step).
//
// PREDICTED: ownerMapPartitionsClaimable falsifies (two members in
// the scheduler's view + one claimable job ⇒ |owners| = 2 ≠ 1) and
// serveSliceDisjointWhenFresh falsifies (every member SERVES the
// whole head — the serve plane overlaps on fresh-owned jobs; this
// kill runs ALONGSIDE the ownership kill: the convoy lane is the
// as-built world, not the isolation proof — the eager lane's twin is
// the isolated disjointness kill). serveSliceCoversFreshOwned HOLDS
// here (everyone serves everything, so owners are served too).
// This is the recorded live shape: N replicas race the head job, one
// wins, N−1 burn their pass (95.6% of claim attempts wasted; 29.1 s
// fleet-wide zero-throughput stall; KEDA scale-out adds racers).
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibSharedHeadConvoy \
//     --invariant=ownerMapPartitionsClaimable \
//     docs/spec/models/calibration/matdist-shared-head-convoy.qnt
//   quint run --backend=rust --main=matDistCalibSharedHeadConvoy \
//     --invariant=serveSliceDisjointWhenFresh \
//     docs/spec/models/calibration/matdist-shared-head-convoy.qnt
module matDistCalibSharedHeadConvoy {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = true,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false
  ).* from "../materializationDistribution"
}
