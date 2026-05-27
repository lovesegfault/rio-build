// -*- mode: Bluespec; -*-
//
// Acceptance re-run, the `937a9c928` completion-clobber member, against
// the REPLACEMENT model (refcount-formal plan T-1a.6; the member the
// Stage-C Phase-1 input list added to the design's G4a/G5 re-run set):
// completion not claim-gated, re-pointed from the as-built model
// (calibration/refcount-g1.qnt :: refcountCalibG1CompletionUnclaimGated)
// at chunkCollect.qnt. The completion claim gate survives the
// replacement as a path-row janitor mechanism and its falsifiable
// content (premature visibility of an in-flight successor) is unchanged
// by the counter's removal; if reverting it no longer falsified here,
// the replacement model would be too coarse to guard it and Wave A2
// must not start (plan T-1a.6 step 5).
//
// EVIDENCE MODULE ONLY — committed, typechecked with the tree,
// re-runnable; not wired as a CI check. Verdicts, depths, and state
// counts are recorded in the introducing commit's message and the
// invariant map's acceptance table.
//
// Reaching the clobber state needs an owner that misses heartbeats
// while alive (the progress-driven-heartbeat era), i.e. the
// contract-free clock: ENABLE_HB_CONTRACT = false. The claim gate is
// the mechanism under test; the distinguishing control is the default
// `step` over this same instantiation (as-designed claim-gated
// completion under the same relaxed clock), predicted to HOLD both the
// local ownership restatement and CR-1 — note this is a STRONGER
// baseline than the Stage-C as-built one, which could not hold CR-1
// under the relaxed clock because of the late-mark window: the T-pre.1
// deleted-guard carried by the replacement model closes exactly that
// window (the wired chunkCollectLateMarkGuarded check is the
// full-alphabet form of the same statement).
//
// PREDICTED: completionRequiresCurrentOwner (the module-local ownership
// restatement, same text as Stage C's) falsifies under calibStep — a
// stale uploader flips the successor's in-flight placeholder to
// 'complete' — and cr1NoLiveChunkCollected falsifies as the harm-level
// consequence (the foreign flip makes the successor's still-uploading
// manifest readable while its chunks' PUTs have not happened).
//
// Re-run commands (serial):
//
//   quint verify --backend=tlc --main=refcountCollectG1CompletionUnclaimGated \
//     --step=calibStep --invariant=completionRequiresCurrentOwner \
//     docs/spec/models/calibration/refcount-collect-g1-completion.qnt
//
//   quint verify --backend=tlc --main=refcountCollectG1CompletionUnclaimGated \
//     --step=calibStep --invariant=cr1NoLiveChunkCollected \
//     docs/spec/models/calibration/refcount-collect-g1-completion.qnt
//
//   quint verify --backend=tlc --main=refcountCollectG1CompletionUnclaimGated \
//     --invariant=completionRequiresCurrentOwner,cr1NoLiveChunkCollected \
//     docs/spec/models/calibration/refcount-collect-g1-completion.qnt

module refcountCollectG1CompletionUnclaimGated {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
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
    ENABLE_HB_CONTRACT = false,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).* from "../chunkCollect"

  /// PRE-FIX completion: admitted by row existence alone (status =
  /// 'uploading'), no claim_id filter — `complete_manifest_in_conn`
  /// before 937a9c928.
  action completeManifestUnclaimGated(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UMarked,
      paths.get(p).status == PUploading,
      paths' = paths.set(p, { ...paths.get(p), status: PComplete, staleness: 0 }),
      ups' = ups.set(u, { ...ups.get(u), pc: UDone }),
      chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
    }
  }

  /// PRE-FIX completion, no-row arm: the UPDATE matches nothing (the
  /// placeholder is gone or already complete) and the handler falls
  /// through to its cleanup path.
  action completeNoRowPreClaim(u: int): bool = all {
    ups.get(u).pc == UMarked,
    paths.get(ups.get(u).path).status != PUploading,
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// Local restatement of the ownership content of the completion claim
  /// gate (textually the Stage-C module's, the path-level S4 form for
  /// this revert): a path is never 'complete' while the uploader
  /// holding its CURRENT placeholder generation is still in a
  /// pre-completion phase. As designed (and under the default `step`
  /// baseline) the only action that flips a row to 'complete' is the
  /// claim-gated completion, performed by the current generation's
  /// owner in the same transition that retires it — so this holds;
  /// dropping the claim gate lets a stale uploader flip the successor's
  /// in-flight row and falsifies it.
  val completionRequiresCurrentOwner = PATHS.forall(p =>
    paths.get(p).status == PComplete implies
      not(UPLOADERS.exists(u => and {
        ups.get(u).path == p,
        ups.get(u).claim == paths.get(p).gen,
        Set(UClaimed, UTxOpen, UUpgraded, UPutDone, UMarked).contains(ups.get(u).pc),
      })))

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
      completeManifestUnclaimGated(u),
      completeNoRowPreClaim(u),
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
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tick,
    }
  }

  /// The documented incident shape, replayed deterministically: the
  /// first owner stalls after marking (the contract-free clock lets its
  /// row go stale), the hot-path reclaim deletes the row, a successor
  /// re-claims the path and stages a larger manifest whose new chunk
  /// has not been uploaded yet, and the stale owner's unclaim-gated
  /// completion then flips the successor's in-flight placeholder to
  /// 'complete' — readable with a missing chunk.
  run g1CompletionClobberRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(tick)
      .then(tick)
      .expect(hotpathStale(1))
      .then(hotPathReclaim(1))
      .then(claimPath(2, 1))
      .then(upgradeManifest(2, Set(1, 2)))
      .expect(all {
        // the successor's chunk 2 is staged but not uploaded; chunk 1
        // is deduplicated against confirmed presence (CR-4-compliant)
        ups.get(2).needsUpload == Set(2),
        not(s3.contains(2)),
        completionRequiresCurrentOwner,
      })
      .then(completeManifestUnclaimGated(1))
      .expect(all {
        // the stale owner flipped the successor's in-flight row
        paths.get(1).status == PComplete,
        not(completionRequiresCurrentOwner),
        not(cr1NoLiveChunkCollected),
      })
}
