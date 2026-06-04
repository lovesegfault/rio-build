// -*- mode: Bluespec; -*-
//
// Tail-reader calibration, merged_bug_130: the orphaned relay keeps
// re-opening.
//
// PRE-FIX behavior modeled: the dead-drain-sender error
// (drain.changed() Err / has_changed() Err) was mapped to naturalEnd,
// whose verdict re-opens whenever the log is incomplete — and with no
// consumer left, the grace never arms and the backoff was skipped:
// the orphaned relay hot-looped opens at ~8,500/s against the store.
//
// PREDICTED: orphanNeverReopens falsifies — the open count climbs past
// its orphan-time checkpoint.
//
// Run (falsification):
//   quint verify --backend=tlc --main=tailReaderCalibOrphanHotloop \
//     --step=calibStep --invariant=orphanNeverReopens \
//     docs/spec/models/calibration/tailreader-orphan-hotloop.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=tailReaderCalibOrphanHotloop \
//     --invariant=orphanNeverReopens \
//     docs/spec/models/calibration/tailreader-orphan-hotloop.qnt
module tailReaderCalibOrphanHotloop {
  import tailReaderLoop.* from "../tailReaderLoop"

  /// PRE-FIX: the orphan cause is invisible (mapped to naturalEnd), so
  /// the reopen arm stays enabled after the drain sender died.
  action calibReopenWhileOrphaned = all {
    orphanSignal, not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = min2(pacerLevel + 1, PACER_MAX_LEVEL),
    sinceProgress' = sinceProgress + 1,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  action calibStep = any {
    step,
    calibReopenWhileOrphaned,
  }
}
