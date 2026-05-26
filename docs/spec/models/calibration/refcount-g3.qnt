// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G3: "the counter was used as an
// S3-presence signal" (data loss) — the pre-M_033 dedup semantics
// (refcount-inventory.md §5 G3, refcount-formal-design.md §3.4; verdicts
// in the calibration table of docs/spec/models/refcount-invariant-map.md).
// Same extend-and-override pattern as refcount-g1.qnt. The second module
// is the design's pre-registered G2×G3 joint-revert row (the I-040
// stale-skip production trace), not a single-commit revert.
//
// Family disposition of the commits NOT carried by an override module:
//   - `dd5c11376` (M_033) — ENC, refcountCalibG3CounterAsPresence below.
//   - `b1c7a9497` — ENC-A: the pre-fix re-query race (the dedup verdict
//     read in a separate statement after the upsert) only loses data
//     under the counter-as-presence semantics this family reverts; at
//     model resolution both shapes produce the same harm state — a
//     writer skips the PUT for a chunk whose presence was never
//     confirmed — which is exactly what the M_033 override falsifies.
//     The atomicity content (verdict computed in the same statement as
//     the upsert) is what the as-built upgradeManifest encodes and what
//     the surviving store.cas.upsert-inserted+2 rule pins.
//   - `127168477` — NOT-ENCODED (set-collapsed chunk_list and SQL-error
//     granularity; pre-registered per-commit exception, design §3.4 —
//     covered by the upsert dedup unit test and the manifest_deserialize
//     fuzz target).
//   - `00fd5b12d` — SUBSUMED: the PutChunk RPC that failed to set
//     uploaded_at was deleted (c5bb34612); the subject no longer exists.

// =========================================================================
// CALIBRATION — pre-dd5c11376 (M_033): the needs-upload verdict is keyed
// on the liveness record instead of confirmed backend presence — a chunk
// whose row already exists is "already uploaded by someone else", so the
// writer skips the PUT. The historical harm (2026-04-06 production data
// loss): the "someone else" had been SIGKILLed mid-PUT, so PG said the
// chunk existed while S3 had nothing.
// PREDICTED: cr4PresenceFromConfirmedUpload falsifies at the second
// writer's upgrade (a PUT skipped for a chunk with no confirmed
// presence), then cr1NoLiveChunkCollected falsifies when that writer
// completes (a 'complete' manifest referencing a chunk with no backend
// object — the data-loss state, reachable with or without the
// SIGKILL because the skip happens before the first writer's PUT either
// way; the crash alphabet keeps the production shape reachable).
// Constants are the crash regime's verbatim, so the Stage-B crash
// verdict is the baseline.
// =========================================================================
module refcountCalibG3CounterAsPresence {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = true,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX upgrade transaction: identical to the as-built action
  /// except the needs-upload verdict — a chunk needs a PUT only if its
  /// row did not already exist (presence inferred from the liveness
  /// record), never read from uploaded_at. The CR-4 sensor (the
  /// skippedPutWithoutPresence ghost) is unchanged: it records any
  /// verdict that skips a chunk whose presence is unconfirmed.
  action upgradeManifestCounterDedup(u: int, list: Set[int]): bool = {
    val p = ups.get(u).path
    val needs = list.filter(h => not(chunks.get(h).present))
    all {
      ups.get(u).pc == UClaimed,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      paths' = paths.set(p, { ...paths.get(p), mdata: Md(list) }),
      chunks' = upsertChunks(chunks, list),
      ups' = ups.set(u, { ...ups.get(u), pc: UUpgraded, list: list, needsUpload: needs, tokenStale: false }),
      wit' = { ...wit, skippedPutWithoutPresence: or {
        wit.skippedPutWithoutPresence,
        list.exists(h => not(needs.contains(h)) and not(chunks.get(h).uploadedAt)),
      } },
      s3' = s3, outbox' = outbox, orphanCand' = orphanCand, leakedRefs' = leakedRefs,
    }
  }

  action calibStep = {
    nondet u = UPLOADERS.oneOf()
    nondet p = PATHS.oneOf()
    nondet h = HASHES.oneOf()
    nondet list = LIST_CHOICES.oneOf()
    any {
      claimPath(u, p),
      upgradeManifestCounterDedup(u, list),
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
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }
}

// =========================================================================
// CALIBRATION — the pre-registered G2×G3 joint revert (e5bdbff1b +
// dd5c11376 together): the I-040 stale-skip production trace. The
// inline-only reap leaves a stale refcount behind a deleted manifest;
// the counter-as-presence dedup then reads that stale record as "someone
// else uploaded this" and skips the PUT the chunk actually needs.
// PREDICTED: cr4PresenceFromConfirmedUpload falsifies at the re-upload's
// skip, then cr1NoLiveChunkCollected falsifies when the re-upload
// completes — the GetPath chunk-not-found data loss, mechanically.
// Constants are the contend regime's verbatim (the abandoned first
// upload is an explicit abort; no crash is needed), so the Stage-B
// contend verdict is the baseline.
// =========================================================================
module refcountCalibG3JointStaleSkip {
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

  /// PRE-FIX owner-side reap (the e5bdbff1b half): manifest rows
  /// deleted, chunk accounting untouched. Same encoding as
  /// refcountCalibG2ReapInlineOnly.
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

  /// PRE-FIX upgrade transaction (the dd5c11376 half): presence inferred
  /// from the liveness record. Same encoding as
  /// refcountCalibG3CounterAsPresence.
  action upgradeManifestCounterDedup(u: int, list: Set[int]): bool = {
    val p = ups.get(u).path
    val needs = list.filter(h => not(chunks.get(h).present))
    all {
      ups.get(u).pc == UClaimed,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      paths' = paths.set(p, { ...paths.get(p), mdata: Md(list) }),
      chunks' = upsertChunks(chunks, list),
      ups' = ups.set(u, { ...ups.get(u), pc: UUpgraded, list: list, needsUpload: needs, tokenStale: false }),
      wit' = { ...wit, skippedPutWithoutPresence: or {
        wit.skippedPutWithoutPresence,
        list.exists(h => not(needs.contains(h)) and not(chunks.get(h).uploadedAt)),
      } },
      s3' = s3, outbox' = outbox, orphanCand' = orphanCand, leakedRefs' = leakedRefs,
    }
  }

  action calibStep = {
    nondet u = UPLOADERS.oneOf()
    nondet p = PATHS.oneOf()
    nondet h = HASHES.oneOf()
    nondet list = LIST_CHOICES.oneOf()
    any {
      claimPath(u, p),
      upgradeManifestCounterDedup(u, list),
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

  /// The documented I-040 incident shape, replayed deterministically:
  /// the first upload is abandoned before its PUT and reclaimed with the
  /// inline-only delete (manifest gone, refcount and bare chunk row left
  /// behind); the re-upload of the same content then trusts the stale
  /// record, skips the PUT, and completes a manifest whose chunk has no
  /// backend object.
  run g3JointStaleSkipRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifestCounterDedup(1, Set(1)))
      .then(abortToReap(1))
      .then(reapByClaimInlineOnly(1))
      .expect(all {
        paths.get(1).status == PNone,
        chunks.get(1).rc == 1,
        refs(1) == Set(),
        not(chunks.get(1).uploadedAt),
        not(s3.contains(1)),
        not(cr3CounterRefinesFold),
      })
      .then(claimPath(2, 2))
      .then(upgradeManifestCounterDedup(2, Set(1)))
      .expect(all {
        ups.get(2).needsUpload == Set(),
        wit.skippedPutWithoutPresence,
        not(cr4PresenceFromConfirmedUpload),
      })
      .then(uploadChunks(2))
      .then(markUploaded(2))
      .then(completeManifest(2))
      .expect(all {
        paths.get(2).status == PComplete,
        not(s3.contains(1)),
        not(cr1NoLiveChunkCollected),
      })
}
