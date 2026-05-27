// -*- mode: Bluespec; -*-
//
// Acceptance re-run, family G4a, against the REPLACEMENT model
// (refcount-formal plan T-1a.6; design §4.6 "the calibration table is
// re-run as the acceptance table"): the drain re-check / resurrect
// handling reverted to its pre-`aa738a5d7` (M_006) shape, re-pointed
// from the as-built model (calibration/refcount-g4a.qnt, Stage C) at
// chunkCollect.qnt. The drain re-check is a SURVIVING mechanism of the
// replacement (design §4.2 — under Release A it narrows to `deleted`
// only); if reverting it no longer falsified CR-1 here, the
// replacement model would be too coarse to guard it and Wave A2 must
// not start (plan T-1a.6 step 5).
//
// EVIDENCE MODULE ONLY — committed, typechecked with the tree,
// re-runnable; not wired as a CI check (the wired
// quint-refcount-calib-g4a-drain-recheck check against the as-built
// model keeps guarding the mechanism until Phase 2 re-points it).
// Verdicts, depths, and state counts are recorded in the introducing
// commit's message and the invariant map's acceptance table.
//
// PREDICTED: cr1NoLiveChunkCollected falsifies — a chunk collected by
// the cycle and then resurrected by a re-referencing upgrade loses its
// backend object when the pre-fix drain processes the stale outbox row
// without re-checking the chunk's current state.
// Constants are the wired contend regime's verbatim, so that regime's
// exhaustive HOLDS (quint-chunk-collect-contend) is the baseline.
//
// Re-run command (serial; the apalache-server prelude of nix/quint.nix
// applies when run inside the sandbox, plain `quint verify` outside):
//
//   quint verify --backend=tlc --main=refcountCollectG4aDrainNoRecheck \
//     --step=calibStep --invariant=cr1NoLiveChunkCollected \
//     docs/spec/models/calibration/refcount-collect-g4a.qnt

module refcountCollectG4aDrainNoRecheck {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
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

  /// PRE-FIX drain row transaction: pop the row, DeleteObject, done —
  /// no `deleted ... FOR UPDATE` re-check, no resurrection skip. The
  /// CR-1 sensors (s3Deleted, deletedWhileReferenced) are unchanged.
  action drainOnePreRecheck(h: int): bool = all {
    outbox.contains(h),
    s3' = s3.exclude(Set(h)),
    outbox' = outbox.exclude(Set(h)),
    wit' = { ...wit,
      s3Deleted: true,
      deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
    },
    paths' = paths, chunks' = chunks, ups' = ups, cycle' = cycle,
  }

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
      heartbeat(u),
      reapByClaim(u),
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      collectSnapshot,
      collectMark,
      collectSweep(h),
      collectFinish,
      quarantineCorrupt(p),
      drainOnePreRecheck(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}
