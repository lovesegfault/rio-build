// -*- mode: Bluespec; -*-
//
// Falsify twin, controller wedge family (bughunt-2 slot 5,
// merged_bug_009). Live-import form: the AS-BUILT defect is an axis of
// the live wedgeCluster model (ENABLE_POPULATIONS = false), not a
// frozen copy — the twin breaks loudly if the live transition relation
// drifts.
//
// PRE-FIX behavior modeled: the systemic guard compares
// retained-evidence wedged nodes against the THIS-TICK fleet only
// (incommensurable populations), escapes entirely on an empty fleet,
// and an RPC-failed tick runs the full verdict over an empty view.
//
// PREDICTED: affectedLeOf falsifies within 2 ticks — tick 1 anchors
// two nodes' evidence inside a wide fleet (per-node verdict), tick 2
// observes a 1-node fleet while both retained nodes still qualify:
// Systemic{affected: 2, of: 1}. (The empty-fleet escape variant
// surfaces as noPerNodeFromSuppressedEvidence in the
// wedge-176-early-return twin, where the undrained episode supplies
// the retained anchors.)
//
// Recorded violation seed (quint run, --backend=rust):
//   0xa0fd4de6691f8839 — see the slot-5 wave-log entry.
//
// Run:
//   quint run --main=wedgeCalib009SplitPopulation \
//     --invariant=affectedLeOf \
//     docs/spec/models/calibration/wedge-009-split-population.qnt
module wedgeCalib009SplitPopulation {
  import wedgeCluster(
    MAX_TIME = 5,
    WINDOW = 3,
    MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = false,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EVICTION = true,
  ENABLE_EPISODE_LATCH = false,
    TRACK_EPISODE_GHOSTS = false
  ).* from "../wedgeCluster"
}
