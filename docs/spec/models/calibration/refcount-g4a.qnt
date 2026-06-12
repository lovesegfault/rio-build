// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G4a: "collect raced a concurrent
// re-reference" — the chunk-level GC/drain TOCTOU vs a re-upload
// (refcount-inventory.md §5 G4 chunk-level rows, refcount-formal-design.md
// §3.4 G4a; verdicts in the VERDICT ROWS block at the end of this
// header — full table: docs/spec/models/refcount-records.md). Same extend-and-override
// pattern as refcount-g1.qnt. The path-level mark/sweep rows of G4
// (G4b) are pre-registered NOT-ENCODED — path unreachability is an
// abstract environment choice in chunkLiveness.qnt — and stay with the
// store.gc.* rules and their tests.
//
// Family disposition of the commits NOT carried by an override module:
//   - `aa738a5d7` (M_006) — ENC, refcountCalibG4aDrainNoRecheck below
//     (the drain re-check half; the resurrect arm of the same commit is
//     left as built — reverting the re-check alone is the minimal delta
//     that re-opens the race the commit closed).
//   - `a2d4c6cd8` — ENC-A (drain-re-check-lock half): the pre-fix
//     re-check ran without FOR UPDATE, so its verdict could be stale at
//     DeleteObject time; at the model's one-action-per-SQL-transaction
//     granularity that staleness window collapses onto "the delete does
//     not observe the concurrent resurrect", which is exactly the state
//     the no-re-check override below reaches. The path_tenants /
//     cycle-reclaim halves of the same commit are G4b (NOT-ENCODED).
//   - `2b68855c5`, `261e78c9d`, `7d5ff71dc`, `62851c73d`, `132446e7e`,
//     `5ba946682`, `adfd303d7` C1/C3, `bf7e516e4` C5 — G4b, NOT-ENCODED
//     as pre-registered (the path-level reachability GC: mark CTE,
//     sweep re-check, referrer ordering, resurrection settling,
//     path_tenants); covered by store.gc.two-phase,
//     store.gc.sweep-recheck+2, store.gc.sweep-referrer-order,
//     store.gc.sweep-cycle-reclaim, store.gc.tenant-retention and the
//     sweep/mark test suites.

// =========================================================================
// CALIBRATION — pre-aa738a5d7 (M_006): the outbox drain deletes the
// backend object for every row it picks up, with no same-transaction
// re-check of the chunk's current state — a chunk resurrected by a
// re-upload between the soft-delete that enqueued it and the drain pass
// still loses its object. The historical harm: GC of a path racing a
// re-upload of the same content deleted a chunk the new manifest had
// just deduplicated against.
// PREDICTED: cr1NoLiveChunkCollected falsifies — the action form first
// (a DeleteObject issued while a manifest references the chunk), the
// state form one completion later. Constants are the contend regime's
// verbatim, so the Stage-B contend verdict is the baseline.
// =========================================================================
//
// VERDICT ROWS (Stage-C, relocated from the retired refcount invariant
// map; full table with the NOT-ENC/SUBS dispositions:
// docs/spec/models/refcount-records.md):
// - refcountCalibG4aDrainNoRecheck (aa738a5d7 / M_006, ENC): FALSIFIES
//   cr1NoLiveChunkCollected @ calibStep (depth 7, 63,334/4,030) — a re-
//   upload resurrects the enqueued chunk and the pre-fix drain deletes
//   its object while referenced. Baseline: contend-regime constants
//   verbatim (Stage-B exhaustive HOLDS).
// - ENC-A by analogy: a2d4c6cd8 (drain re-check half) covered by
//   refcountCalibG4aDrainNoRecheck — at one-action-per-SQL-transaction
//   granularity the lockless re-check's staleness window collapses onto
//   "the delete does not observe the concurrent resurrect".
module refcountCalibG4aDrainNoRecheck {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX drain row transaction: pop the row, DeleteObject, done —
  /// no `(deleted AND refcount = 0) ... FOR UPDATE` re-check, no
  /// resurrection skip. The CR-1 sensors (s3Deleted,
  /// deletedWhileReferenced) are unchanged.
  action drainOnePreRecheck(h: int): bool = all {
    outbox.contains(h),
    s3' = s3.exclude(Set(h)),
    outbox' = outbox.exclude(Set(h)),
    wit' = { ...wit,
      s3Deleted: true,
      deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
    },
    paths' = paths, chunks' = chunks, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs,
  }

  action calibStep = {
    nondet u = UPLOADERS.oneOf()
    nondet p = PATHS.oneOf()
    nondet h = HASHES.oneOf()
    nondet list = LIST_CHOICES.oneOf()
    any {
      claimPath(u, p),
      upgradeManifest(u, list),
      uploadChunks(u),
      markUploaded(u),
      completeManifest(u),
      completeFailsForeign(u),
      abortToReap(u),
      uploadFailsToRollback(u),
      heartbeat(u),
      rollbackTx(u),
      reapByClaim(u),
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      pathSweepBatch,
      orphanSelect,
      orphanSweepBatch,
      drainOnePreRecheck(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}
