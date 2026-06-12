// -*- mode: Bluespec; -*-
//
// Round-12 WO-S9-8 calibration twin (live_061, the dead-head face of
// sched.materialize.claimability-projection): a DEAD job — its node
// completed by other means while the job sat unresolved, so every
// claim against it answers Gone and it can never convert — is STILL
// SERVED: no node-face exclusion at the listing (the as-built pre-fix
// world: the db query was node-blind and the serve filter read
// claimability only). The dead head then pins every serve set it
// lands in: claim attempts burn on it forever, the oldest-first
// window never drains (live_061: 10,876 Gone-answers/78s, ~0.5%
// fleet conversion). The live law (the rust listing predicate + the
// in-memory serve filter) removes a dead job from the servable plane
// at the listing itself; its terminal disposition (the obsolete
// fold) is the job model's plane, not this model's. Const lane of
// the live module; no action overrides ((cc)/(ccccc): all consts
// bound, default step).
//
// PREDICTED: servedJobsConvertibleWithinHorizon falsifies — die j0,
// let `now` advance past the steal-horizon grace while a member's
// beat still serves it: the served set carries a dead job older than
// the grace. The sibling invariants HOLD in this very lane (the
// quint-matdist-dead-siblings-intact holds check is the
// kill-isolation witness: the servable plane is the raw claimable
// set under the defect, which is exactly the base model's plane, and
// the convertibility guard only RESTRICTS claims — every sibling's
// reachable projection stays inside the base census).
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibDeadHeadPinsWindow \
//     --invariant=servedJobsConvertibleWithinHorizon \
//     docs/spec/models/calibration/matdist-dead-head-pins-window.qnt
module matDistCalibDeadHeadPinsWindow {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false,
    WEDGE_AXIS_ENABLED = false,
    WEDGED_KEEPS_BEATING = false,
    DEAD_AXIS_ENABLED = true,
    DEAD_HEAD_SERVED = true
  ).* from "../materializationDistribution"
}
