// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G2: "a cleanup path forgot the
// chunks" (leaked refcounts) — the missed / collapsed decrement class
// (refcount-inventory.md §5 G2, refcount-formal-design.md §3.4; verdicts
// in the calibration table of docs/spec/models/refcount-invariant-map.md).
// Same extend-and-override pattern as refcount-g1.qnt.
//
// Family disposition of the commits NOT carried by an override module:
//   - `e5bdbff1b` (I-040) — ENC, refcountCalibG2ReapInlineOnly below.
//   - `dbb42232a` — ENC-A: the same inline-only-delete shape at a
//     different call site (abort_upload, the batch drop path); the
//     model's writers funnel every owner-side cleanup through the same
//     reap/rollback actions, so the e5bdbff1b override is the same
//     revert at model resolution.
//   - `adfd303d7` C2 — ENC, refcountCalibG2SweepCollapsedCount below.
//   - `d617bf3e5` — split: the M_023 CHECK half is a passive schema
//     constraint whose model image (m023NonNegative) is an invariant in
//     every regime, not a mechanism an override can revert — the
//     under-count class it detects is demonstrated by the G1 pre-token
//     override driving a counter below zero; the orphan-chunk-sweep
//     wiring half is NOT-ENCODED (the existence/cadence of a background
//     collection loop is below the structural CR-2 encoding, the same
//     pre-registered treatment as the 15-minute/hourly cadences —
//     coverage stays with the sweep unit tests and the wired
//     orphan-sweep witnesses).
//   - `8d93ce6c1` — SUBSUMED: the chunk_tenants junction it cleaned up
//     was dropped by migration 035; the subject no longer exists in the
//     tree.

// =========================================================================
// CALIBRATION — pre-e5bdbff1b (I-040): the owner-side reap uses the
// inline-only delete — the placeholder's manifest rows are deleted but
// its chunk accounting is never touched (no chunk_list parse, no
// decrement, no zero-detect, no enqueue). The historical harm: the
// substituter's reclaim leaked every reclaimed upload's refcounts;
// chunks whose references were gone stayed at refcount >= 1 forever.
// PREDICTED: cr3CounterRefinesFold falsifies at the first inline-only
// reap of an upgraded placeholder (counter exceeds the fold), and
// cr2NoStrandedGarbage falsifies in the same state (an unreferenced,
// undeleted chunk stuck above zero is permanent garbage — the leak).
// Restricted alphabet (one uploader, one path, one hash) ⇒ the as-built
// default `step` over this module's constants is the baseline, recorded
// as HOLDS in the table.
// =========================================================================
module refcountCalibG2ReapInlineOnly {
  import chunkLiveness(
    MAX_HASHES = 1,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX owner-side reap: claim-gated exactly as built (that gate is
  /// not the mechanism under test here), but the admitted branch deletes
  /// only the manifest rows — the decrement / zero-detect / enqueue
  /// family never runs.
  action reapByClaimInlineOnly(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UReaping,
      if (claimReapAdmitted(p, ups.get(u).claim)) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
        leakedRefs' = leakedRefs, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        wit' = { ...wit, lateCleanupNoop: true },
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox,
        orphanCand' = orphanCand, leakedRefs' = leakedRefs,
      },
    }
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
      reapByClaimInlineOnly(u),
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      pathSweepBatch,
      orphanSelect,
      orphanSweepBatch,
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}

// =========================================================================
// CALIBRATION — pre-adfd303d7 C2: the path-GC sweep batch decrements
// each referenced hash by AT MOST ONE per batch instead of by the number
// of dying manifests that reference it — a chunk shared by N swept paths
// loses one reference, not N. The historical harm: shared chunks stuck
// at refcount >= 1 with no surviving references, a permanent S3 leak.
// PREDICTED: cr3CounterRefinesFold falsifies after one batch sweep of
// two complete manifests sharing a chunk (counter 1, fold 0), and
// cr2NoStrandedGarbage falsifies in the same state (the stranded shape).
// Constants are the contend regime's verbatim, so the Stage-B contend
// verdict is the baseline.
// =========================================================================
module refcountCalibG2SweepCollapsedCount {
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

  /// PRE-FIX batch sweep: both dying manifests' parsed hash sets are
  /// collapsed to a DISTINCT list before the decrement, so every
  /// referenced hash is decremented by exactly one regardless of how
  /// many dying manifests reference it. Zero-detect and enqueue are the
  /// as-built statements over that (wrong) decrement.
  action pathSweepBatchCollapsed: bool = {
    val counts = HASHES.mapBy(h =>
      if (PATHS.exists(p => parsedListOf(p).contains(h))) 1 else 0)
    val res = decrementAndZero(chunks, counts)
    all {
      PATHS.forall(p => paths.get(p).status == PComplete),
      PATHS.size() >= 2,
      paths' = PATH_KEYS.mapBy(p => emptyPath(paths.get(p).gen)),
      chunks' = res.cm,
      outbox' = outbox.union(res.zeroed),
      leakedRefs' = leakedRefs.keys().mapBy(h =>
        leakedRefs.get(h) +
          PATHS.filter(p => listOf(p).contains(h) and not(parsedListOf(p).contains(h))).size()),
      ups' = ups, s3' = s3, orphanCand' = orphanCand, wit' = wit,
    }
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
      pathSweepBatchCollapsed,
      orphanSelect,
      orphanSweepBatch,
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}
