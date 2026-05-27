// -*- mode: Bluespec; -*-
//
// Acceptance re-run, family G5, against the REPLACEMENT model
// (refcount-formal plan T-1a.6; design §4.6): the heartbeat removed —
// the pre-`a1b49b4a3` shape — re-pointed from the as-built model
// (calibration/refcount-g5.qnt, Stage C) at chunkCollect.qnt. The
// heartbeat survives the replacement as a path-row janitor mechanism
// (design §4.2); if removing it no longer falsified S5 in its
// path-level form here, the replacement model would be too coarse to
// guard it and Wave A2 must not start (plan T-1a.6 step 5).
//
// EVIDENCE MODULES ONLY — committed, typechecked with the tree,
// re-runnable; not wired as CI checks (the wired
// quint-refcount-calib-g5-no-heartbeat check against the as-built
// model keeps guarding the mechanism until Phase 2 re-points it).
// Verdicts, depths, and state counts are recorded in the introducing
// commit's message and the invariant map's acceptance table.
//
// Encoding note: in chunkCollect.qnt the production heartbeat contract
// is the ENABLE_HB_CONTRACT constant (a tick precondition), so "no
// heartbeat exists" is that constant set to false PLUS the heartbeat
// action absent from the step relation — without a heartbeat statement
// there is no contract for the clock to enforce and nothing for a live
// owner to call. The restricted alphabet (one uploader, one path) is
// not a wired regime, so the distinguishing baseline is the second
// module below: the as-designed actions (heartbeat present, contract
// enforced) over the same constants, predicted to HOLD S5.
//
// PREDICTED: s5LiveOwnerNeverReaped falsifies under calibStep — a
// stale reclaim reaps a placeholder whose owner is alive in a
// guard-armed phase.
//
// Re-run commands (serial):
//
//   quint verify --backend=tlc --main=refcountCollectG5NoHeartbeat \
//     --step=calibStep --invariant=s5LiveOwnerNeverReaped \
//     docs/spec/models/calibration/refcount-collect-g5.qnt
//
//   quint verify --backend=tlc --main=refcountCollectG5Baseline \
//     --invariant=s5LiveOwnerNeverReaped \
//     docs/spec/models/calibration/refcount-collect-g5.qnt

module refcountCollectG5NoHeartbeat {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = false,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).* from "../chunkCollect"

  /// PRE-FIX step: no heartbeat action exists; the clock is not bound
  /// by any heartbeat contract (ENABLE_HB_CONTRACT = false above).
  action calibStep = {
    nondet u = UPLOADERS.oneOf()
    nondet p = PATHS.oneOf()
    nondet h = HASHES.oneOf()
    nondet list = LIST_CHOICES.oneOf()
    any {
      claimPath(u, p),
      upgradeManifest(u, list),
      upgradeBegin(u, list),
      upgradeCommit(u),
      uploadChunks(u),
      markUploaded(u),
      completeManifest(u),
      completeFailsForeign(u),
      abortToReap(u),
      reapByClaim(u),
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      collectSnapshot,
      collectMark,
      collectSweep(h),
      collectFinish,
      quarantineCorrupt(p),
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}

// The distinguishing baseline over the same restricted alphabet: the
// as-designed actions (heartbeat present, the production contract
// enforced as the tick precondition). PREDICTED: s5LiveOwnerNeverReaped
// HOLDS exhaustively — the falsification above is attributable to the
// heartbeat's removal, not to the alphabet restriction.
module refcountCollectG5Baseline {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).* from "../chunkCollect"
}
