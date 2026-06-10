// -*- mode: Bluespec; -*-
//
// Round-6 WO-S8-4 calibration twin (bug_087's re-aim): the serve
// plane consults a SKEWED owner view — `servePrio` inverts the score,
// so serving and owning pick DIFFERENT members whenever the view has
// two or more. This is the model analog of the CF-2 listing-arm /
// owner-map divergence class (a rendezvous-hash or sanitize-salt
// disagreement between the arm the workers poll through and the map
// the partition law is stated over). Const lane of the live module;
// no action overrides ((cc)/(ccccc): all consts bound, default step).
//
// PREDICTED: serveSliceCoversFreshOwned falsifies — the map owner's
// own beat does NOT serve its fresh-owned job (the skewed serve arm
// picked someone else) — while ownerMapPartitionsClaimable AND
// serveSliceDisjointWhenFresh both HOLD in this very lane (the
// inverted score is equally injective, so ownership stays
// exactly-one and the serve plane stays partitioned — it is just the
// WRONG partition). The quint-matdist-skew-siblings-intact holds
// check is the kill-isolation witness: the coverage kill goes
// through the coverage leaf, not through a sibling.
//
// (stealOnlyAfterOwnerStaleness is NOT claimed intact here: a
// skew-served job is a non-own serve with a fresh real owner, so the
// prematureSteal ghost legitimately latches in this lane — the lane
// models a serve-plane defect, and the ghost correctly reports it.)
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibOwnerServeSkew \
//     --invariant=serveSliceCoversFreshOwned \
//     docs/spec/models/calibration/matdist-owner-serve-skew.qnt
module matDistCalibOwnerServeSkew {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = true
  ).* from "../materializationDistribution"
}
