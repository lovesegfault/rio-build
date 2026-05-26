// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G1: "a late or foreign cleanup
// clobbered someone else's upload" (ownership/identity) — the claim_id /
// PlaceholderToken / claim-gate machinery (refcount-inventory.md §5 G1,
// refcount-formal-design.md §3.4; the verdicts live in the calibration
// table of docs/spec/models/refcount-invariant-map.md).
//
// Protocol (the retry campaign's calibration pattern): each module below
// instantiates the AS-BUILT model `chunkLiveness.qnt`, replaces ONE
// owner-side cleanup entry point with a local PRE-FIX variant (the
// behavior the historical fix removed), and exposes the swapped
// transition relation as `calibStep`. The reference fold `refs(h)` and
// every invariant keep their as-built definitions — they are the oracle,
// not part of the reverted behavior. Where a module restricts the
// alphabet below a Stage-B regime's constants, the as-built baseline over
// the same restriction is the module's default `step` (imported), run
// against the same invariant and recorded as HOLDS in the calibration
// table; modules that reuse a Stage-B regime's constants verbatim cite
// that regime's exhaustive Stage-B verdict as the baseline.
//
// Encoding caveat for this family, recorded once here: the main model's
// S4/L3 forms are admission-predicate regression guards (they quantify
// over `claimReapAdmitted` / `rollbackAdmitted` / `heartbeatAdmitted`
// exactly as the as-built actions use them — the logService authGate
// pattern), so they bind edits to the MAIN model's predicates and are
// structurally unable to falsify from an additive override module. The
// ownership content of a G1 revert therefore falsifies here through its
// state-level consequences — the under-count / presence-flag / premature-
// 'complete' harms (CR-3, M_023, CR-4, CR-1) that the foreign mutation
// produces — which is also where the design's §3.4 prediction ("S4, then
// CR-3") lands at this model's resolution.
//
// Family disposition of the commits NOT carried by an override module
// (one line each; the full table is in the invariant map):
//   - `bf7e516e4` C1  — ENC, refcountCalibG1ReapPathMatched below.
//   - `937a9c928` (completion half) — ENC,
//     refcountCalibG1CompletionUnclaimGated below.
//   - `937a9c928` (heartbeat half) — NOT-ENCODED: the foreign-freshen
//     harm is an eventuality (an abandoned row kept artificially fresh is
//     reaped later, not corrupted), outside this safety model; the
//     ownership content is the same claim-gate discipline the completion
//     half demonstrates, and the main model's l3NoForeignFreshen guards
//     the as-built admission predicate.
//   - `1cd975b90` — ENC, refcountCalibG1RollbackPreToken below.
//   - `ae5f3190b` — NOT-ENCODED (input validation; pre-registered
//     per-commit exception, design §3.4).
//   - `31bd9c512`, `539c2be7c`, `31ce52b14` — ENC-A: the harm class (a
//     reaper acting on a stale view of staleness/status/chunk_list
//     double-decrements or reaps a fresh row) is the same
//     stale-credential shape the two overrides below falsify; the
//     literal pre-fix mechanism (the re-check moved inside the reap
//     transaction) is an intra-transaction read/write split below the
//     model's one-action-per-SQL-transaction granularity.

// =========================================================================
// CALIBRATION — pre-1cd975b90: the in-process rollback decrement (DEC-1)
// carries no PlaceholderToken and takes no FOR UPDATE ownership lock —
// it deletes whatever 'uploading' row the path currently has and
// decrements the roller-back's OWN written hash set against it,
// regardless of whose generation the row is. The historical harm: a
// rollback that lost a race with the stale reclaim + a successor's
// re-claim decremented chunks the successor's manifest still references
// (a second decrement for a generation already paid for by the reclaim).
// PREDICTED: cr3CounterRefinesFold falsifies (under-count — the counter
// drops below the manifest fold; under-counts are never sanctioned), and
// m023NonNegative falsifies one decrement deeper (the same
// double-decrement pushes a hash with no surviving increment below
// zero). Constants are the contend regime's verbatim, so the Stage-B
// contend verdict is the baseline.
// =========================================================================
module refcountCalibG1RollbackPreToken {
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

  /// PRE-FIX DEC-1: admitted by row existence alone (status =
  /// 'uploading'), no generation match, no token-epoch comparison.
  /// Effects are the as-built admitted branch: delete the placeholder's
  /// manifest rows, decrement each of the roller-back's own written
  /// hashes by one, no zero-detect, no enqueue.
  action rollbackTxPreToken(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == URollback,
      if (paths.get(p).status == PUploading) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        chunks' = chunks.keys().mapBy(h =>
          val c = chunks.get(h)
          if (ups.get(u).list.contains(h) and c.present) { ...c, rc: c.rc - 1 } else c),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
        leakedRefs' = leakedRefs, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox,
        orphanCand' = orphanCand, leakedRefs' = leakedRefs, wit' = wit,
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
      rollbackTxPreToken(u),
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

  /// The documented incident shape, replayed deterministically: the
  /// upload fails, the hot-path reclaim repairs the abandoned
  /// accounting, a successor re-claims and re-references the shared
  /// chunk, and the late pre-token rollback then decrements the
  /// successor's reference (chunk 1 under-counted) and re-decrements its
  /// own already-reclaimed reference (chunk 2 driven negative).
  run g1PreTokenDoubleDecrementRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadFailsToRollback(1))
      .then(tick)
      .then(tick)
      .then(hotPathReclaim(1))
      .then(claimPath(2, 1))
      .then(upgradeManifest(2, Set(1)))
      .expect(all {
        chunks.get(1).rc == 1,
        chunks.get(2).rc == 0,
        cr3CounterRefinesFold,
      })
      .then(rollbackTxPreToken(1))
      .expect(all {
        // The foreign rollback erased the successor's placeholder and
        // its in-flight reference (the clobber)...
        paths.get(1).status == PNone,
        chunks.get(1).rc == 0,
        // ...and re-decremented its own already-reclaimed reference,
        // driving the counter below zero (the double decrement).
        chunks.get(2).rc == -1,
        not(cr3CounterRefinesFold),
        not(m023NonNegative),
      })
}

// =========================================================================
// CALIBRATION — pre-937a9c928 (completion half): completion is not
// claim-gated — `complete_manifest_in_conn` flips whatever 'uploading'
// row the path has, so a stale uploader that resumes after its
// placeholder was reaped and re-claimed flips the SUCCESSOR's in-flight
// placeholder to 'complete'. Reaching that state also needs an owner
// that misses heartbeats while alive (at that point in history the
// heartbeat was progress-driven only — a1b49b4a3 — so a stalled PUT
// fan-out heartbeated nothing), encoded as the contract-free clock
// below; the claim gate is the mechanism under test, the baselineStep
// keeps the same clock with the as-built gate.
// PREDICTED: the local ownership invariant
// `completionRequiresCurrentOwner` falsifies (a row flipped 'complete'
// over the in-flight holder of its current generation — the clobber the
// claim gate forbids), and cr1NoLiveChunkCollected falsifies as the
// harm-level consequence (the foreign flip makes the successor's
// still-uploading manifest readable while its chunks' PUTs have not
// happened).
// Restricted alphabet (one path, two uploaders) ⇒ the baselineStep below
// is the distinguishing control, recorded in the table: it HOLDS the
// local ownership invariant (the claim gate is the only thing that
// changes between the two relations). NOTE, found by the verification
// runs and walked against the code: the baseline does NOT hold CR-1 —
// under the contract-free clock the as-built actions already reach a
// data-loss state through an unrelated window (a stale reclaim
// soft-deletes an uploaded-but-unmarked chunk; the still-alive owner's
// late `mark_chunks_uploaded` — `WHERE uploaded_at IS NULL`, no deleted
// or claim guard, chunked.rs — re-asserts uploaded_at on the soft-
// deleted row; the drain then deletes the object; the next writer
// trusts uploaded_at and skips the PUT). The CR-1 falsification is
// therefore attributable to the claim-gate revert only through the
// trace shape (the foreign flip is the shallower counterexample), while
// the local invariant isolates the gate cleanly; the late-mark window
// itself is recorded in the calibration findings and the Phase-1 input
// list (it is excluded as-built solely by the heartbeat contract, the
// same assumption that carries S5).
// =========================================================================
module refcountCalibG1CompletionUnclaimGated {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX completion: admitted by row existence alone (status =
  /// 'uploading'), no claim_id filter.
  action completeManifestUnclaimGated(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UMarked,
      paths.get(p).status == PUploading,
      paths' = paths.set(p, { ...paths.get(p), status: PComplete, staleness: 0 }),
      ups' = ups.set(u, { ...ups.get(u), pc: UDone }),
      chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
      leakedRefs' = leakedRefs, wit' = wit,
    }
  }

  /// PRE-FIX completion, no-row arm: the UPDATE matches nothing (the
  /// placeholder is gone or already complete) and the handler falls
  /// through to its cleanup path.
  action completeNoRowPreClaim(u: int): bool = all {
    ups.get(u).pc == UMarked,
    paths.get(ups.get(u).path).status != PUploading,
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox,
    orphanCand' = orphanCand, leakedRefs' = leakedRefs, wit' = wit,
  }

  /// The contract-free clock: time may pass a live, guard-armed owner's
  /// heartbeat deadline (the progress-driven-heartbeat era — a stalled
  /// owner heartbeats nothing). Same effects as the as-built tick.
  action tickUnforced: bool = all {
    paths' = PATH_KEYS.mapBy(p =>
      val r = paths.get(p)
      if (r.status == PUploading) { ...r, staleness: minInt(r.staleness + 1, STALE_CAP) } else r),
    chunks' = HASHES.mapBy(h =>
      val c = chunks.get(h)
      if (c.present) { ...c, age: minInt(c.age + 1, AGE_CAP) } else c),
    s3' = s3, outbox' = outbox, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// Local restatement of the ownership content of the completion claim
  /// gate (the S4-family form for this revert, the way the retry
  /// calibration carried module-local invariants where the main model's
  /// admission-predicate forms cannot see an override): a path is never
  /// 'complete' while the uploader holding its CURRENT placeholder
  /// generation is still in a pre-completion phase. As built (and under
  /// baselineStep) the only action that flips a row to 'complete' is the
  /// claim-gated completion, performed by the current generation's owner
  /// in the same transition that retires it — so this holds; dropping
  /// the claim gate lets a stale uploader flip the successor's in-flight
  /// row and falsifies it.
  val completionRequiresCurrentOwner = PATHS.forall(p =>
    paths.get(p).status == PComplete implies
      not(UPLOADERS.exists(u => and {
        ups.get(u).path == p,
        ups.get(u).claim == paths.get(p).gen,
        Set(UClaimed, UUpgraded, UPutDone, UMarked).contains(ups.get(u).pc),
      })))

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
      completeManifestUnclaimGated(u),
      completeNoRowPreClaim(u),
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
      tickUnforced,
    }
  }

  /// The distinguishing control: the as-built claim-gated completion
  /// over the SAME alphabet (including the contract-free clock). CR-1
  /// must HOLD here for the calibStep falsification to be attributable
  /// to the dropped claim gate rather than to the clock relaxation.
  action baselineStep = {
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
      drainOne(h),
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      tickUnforced,
    }
  }
}

// =========================================================================
// CALIBRATION — pre-bf7e516e4 C1 (claim_id / ReapBy::Claim): the
// owner-side reap (drop-guard, abort, complete-failure cleanup) matches
// on the path alone — whatever 'uploading' row the path currently has is
// reaped, not the generation the credential was minted for. The
// historical harm: a drop-guard that fired after the scanner had already
// reaped row A and uploader B had re-claimed the path reaped B's
// in-flight manifest and chunk accounting mid-upload.
// PREDICTED: cr4PresenceFromConfirmedUpload falsifies — the foreign reap
// soft-deletes and enqueues the successor's chunk under it; the
// successor then commits presence (uploaded_at) on the soft-deleted row
// and the drain deletes the just-uploaded object, leaving a confirmed-
// present chunk with no backend object (clause b).
// Restricted alphabet (one path, one hash, two uploaders) ⇒ the as-built
// default `step` over this module's constants is the baseline, recorded
// as HOLDS in the table.
// =========================================================================
module refcountCalibG1ReapPathMatched {
  import chunkLiveness(
    MAX_HASHES = 1,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX owner-side reap: admitted by row existence alone (status =
  /// 'uploading'), no claim filter. Effects are the as-built admitted
  /// branch: parse the CURRENT row's chunk_list, delete the manifest
  /// rows, decrement, zero-detect, enqueue.
  action reapByPathPreClaim(u: int): bool = {
    val p = ups.get(u).path
    val parsed = parsedListOf(p)
    val res = decrementAndZero(chunks, countsForOne(parsed))
    all {
      ups.get(u).pc == UReaping,
      if (paths.get(p).status == PUploading) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        chunks' = res.cm,
        outbox' = outbox.union(res.zeroed),
        leakedRefs' = leakedRefs.keys().mapBy(h =>
          if (listOf(p).contains(h) and not(parsed.contains(h)))
            leakedRefs.get(h) + 1
          else leakedRefs.get(h)),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        s3' = s3, orphanCand' = orphanCand, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox,
        orphanCand' = orphanCand, leakedRefs' = leakedRefs, wit' = wit,
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
      reapByPathPreClaim(u),
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
