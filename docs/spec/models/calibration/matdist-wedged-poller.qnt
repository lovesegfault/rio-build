// -*- mode: Bluespec; -*-
//
// Round-6 WO-S1-4 calibration twin (merged_bug_005, the honest-beat
// law store.materialize.honest-beat): a WEDGED worker — alive,
// conversion-incapable (budget pinned by an unanswered mint, resume
// ledger at cap, every fresh mint refused with a conversion-
// disproving rejection) — KEEPS issuing listing beats. Its contact
// stays eternally fresh, so its rendezvous slice never enters anyone
// else's steal horizon and never re-homes: the wedged-but-polling
// worker pins ~1/N of the claimable head fleet-wide. This is the
// as-built pre-fix world; the live law (the rust honest-beat gate in
// rio-store/src/materialize/client.rs) withholds the beat instead,
// so capability loss ages into the steal horizon exactly like
// silence does. Const lane of the live module; no action overrides
// ((cc)/(ccccc): all consts bound, default step).
//
// PREDICTED: wedgedOwnerSliceRecovered falsifies — wedge r1w0 (the
// j0 owner at full membership), let it keep beating past the
// steal-horizon grace while a capable member is in the view: j0 is
// in NO capable member's serve set (the wedged owner stays fresh, so
// the steal arm never opens) and the wedged owner itself cannot
// claim. The six sibling invariants HOLD in this very lane (the
// quint-matdist-wedged-siblings-intact holds check is the
// kill-isolation witness: beats are unchanged from the base model
// when the defect keeps them flowing, and the capability guard only
// RESTRICTS claims, so every sibling's reachable projection is a
// subset of the base census).
//
// Run (falsification):
//   quint run --backend=rust --main=matDistCalibWedgedPoller \
//     --invariant=wedgedOwnerSliceRecovered \
//     docs/spec/models/calibration/matdist-wedged-poller.qnt
module matDistCalibWedgedPoller {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false,
    WEDGE_AXIS_ENABLED = true,
    WEDGED_KEEPS_BEATING = true,
    DEAD_AXIS_ENABLED = false,
    DEAD_HEAD_SERVED = false
  ).* from "../materializationDistribution"
}
