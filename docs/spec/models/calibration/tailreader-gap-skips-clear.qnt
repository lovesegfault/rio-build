// -*- mode: Bluespec; -*-
//
// Tail-reader calibration, merged_bug_006: a serve-then-gap drive
// skipped the witnessed-work clear.
//
// PRE-FIX behavior modeled: the witnessed-work clear (`degraded =
// None`) lived only in the DriveEnd::Ended match arm. A drive that
// relayed lines and THEN withheld at a forward jump returned
// DriveEnd::Gap, where the clear was absent — the user saw fresh
// lines while the degraded episode stayed armed and
// `degraded_notice_sent` stayed latched true.
//
// PREDICTED: degradedImpliesNoRelaySinceArm falsifies — a relay
// reaches the output while the degraded episode survives the step.
// Depth-2 trace: reopenFailed (arm) -> calibReopenServeThenGapNoClr.
//
// Run (falsification):
//   quint verify --backend=tlc --main=tailReaderCalibGapSkipsClear \
//     --step=calibStep --invariant=degradedImpliesNoRelaySinceArm \
//     docs/spec/models/calibration/tailreader-gap-skips-clear.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=tailReaderCalibGapSkipsClear \
//     --invariant=degradedImpliesNoRelaySinceArm \
//     docs/spec/models/calibration/tailreader-gap-skips-clear.qnt
module tailReaderCalibGapSkipsClear {
  import tailReaderLoop.* from "../tailReaderLoop"

  /// PRE-FIX: a serve-then-gap re-open is productive (relays lines,
  /// resets the pacer) but the DriveEnd::Gap arm did NOT clear the
  /// degraded episode. The oracle records the relay-while-armed.
  action calibReopenServeThenGapNoClr = all {
    not(orphanSignal), not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = 0,
    sinceProgress' = 0,
    degradedArmed' = degradedArmed,
    relayedSinceArm' = if (degradedArmed) true else relayedSinceArm,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  action calibStep = any {
    step,
    calibReopenServeThenGapNoClr,
  }
}
