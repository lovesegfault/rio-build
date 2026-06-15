// -*- mode: Bluespec; -*-
//
// Tail-reader calibration, merged_bug_054: the pacer reset on bare
// receipt.
//
// PRE-FIX behavior modeled: the dashboard follow loop reset its
// backoff on receivedThisAttempt — ANY chunk receipt, the store's
// always-final keep-alive arm included. Every re-open against a
// session with no live ingest looked "productive" and the loop held a
// flat ~4 Hz poll forever.
//
// PREDICTED: pacingEscalatesAbsentProgress falsifies — the pacer level
// diverges from the cycles-since-progress oracle on the first
// reset-on-skip cycle.
//
// Run (falsification):
//   quint verify --backend=tlc --main=tailReaderCalibResetOnReceipt \
//     --step=calibStep --invariant=pacingEscalatesAbsentProgress \
//     docs/spec/models/calibration/tailreader-reset-on-receipt.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=tailReaderCalibResetOnReceipt \
//     --invariant=pacingEscalatesAbsentProgress \
//     docs/spec/models/calibration/tailreader-reset-on-receipt.qnt
module tailReaderCalibResetOnReceipt {
  import tailReaderLoop.* from "../tailReaderLoop"

  /// PRE-FIX: a bare-receipt cycle (skip verdict) RESETS the pacer —
  /// receipt treated as progress. The oracle keeps counting.
  action calibReopenSkipResets = all {
    not(orphanSignal), not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = 0,
    sinceProgress' = sinceProgress + 1,
    degradedArmed' = degradedArmed, relayedSinceArm' = relayedSinceArm,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  action calibStep = any {
    step,
    calibReopenSkipResets,
  }
}
