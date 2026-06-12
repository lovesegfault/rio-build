// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, family G5: "the repair loops reaped
// live uploads" — the heartbeat / threshold machinery
// (refcount-inventory.md §5 G5, refcount-formal-design.md §3.4; verdicts
// in the VERDICT ROWS block at the end of this header — full table:
// docs/spec/models/refcount-records.md).
// Same extend-and-override pattern as refcount-g1.qnt.
//
// Family disposition of the commits NOT carried by an override module:
//   - `a1b49b4a3` — ENC, refcountCalibG5NoHeartbeat below.
//   - `064ceadbd` — ENC-A: the wall-clock guard task is the mechanism
//     that makes the heartbeat contract hold for stalled-but-alive
//     owners; its absence is the same no-heartbeat world the a1b49b4a3
//     override encodes (the model does not distinguish progress-driven
//     from wall-clock-driven heartbeats — both are the `heartbeat`
//     action). The inline/slow-ingest claim-plumbing half of the commit
//     is outside the chunked-upload scope of this model.
//   - `2d7e4f9fd` (I-207) — witness-form row per the design's §3.4
//     pre-registration ("a liveness/latency property, encoded as a
//     witness, not an invariant"): refcountCalibG5NoHotpathReclaim below
//     removes the hot-path reclaim and the `noHotpathReclaim` witness
//     stops being violable (the 300 s repair path is gone; only the
//     scanner remains), while the committed safety set stays green over
//     the same alphabet — the revert's harm is latency, not safety,
//     which is why the design encodes it as a witness pair rather than
//     a falsification.
//   - `da351aaff`, `f6bf0a546` — NOT-ENCODED (spawn_monitored
//     operability; pre-registered per-commit exception, design §3.4 —
//     the G7 treatment).

// =========================================================================
// CALIBRATION — pre-a1b49b4a3: no heartbeat exists. An upload that
// outlives the stale threshold is reapable mid-flight — the historical
// harm was a 16-minute 6 GB upload reaped at the 15-minute scanner
// threshold, observed as the uploader's completion failing and its
// upload restarting from scratch. Encoded as: the heartbeat action is
// absent from the alphabet and the clock is not bound by the heartbeat
// contract (`tickNoHeartbeat` below) — without a heartbeat statement
// there is no contract for the tick precondition to enforce.
// PREDICTED: s5LiveOwnerNeverReaped falsifies — a stale reclaim reaps a
// placeholder whose owner is alive in a guard-armed phase.
// Restricted alphabet (one uploader, one path) ⇒ the as-built default
// `step` over this module's constants is the baseline, recorded as
// HOLDS in the table.
// =========================================================================
//
// VERDICT ROWS (Stage-C, relocated from the retired refcount invariant
// map; full table with the NOT-ENC/SUBS dispositions:
// docs/spec/models/refcount-records.md):
// - refcountCalibG5NoHeartbeat (a1b49b4a3, ENC): FALSIFIES
//   s5LiveOwnerNeverReaped @ calibStep (depth 4, 152/35) — the hot-path
//   reclaim reaps a live, guard-armed owner. Baseline (as-built step,
//   same constants): HOLDS (9,705/981). Complements the Stage-B
//   threshold-order check: that one inverts the ordering, this one
//   removes the heartbeat itself.
// - refcountCalibG5NoHotpathReclaim (2d7e4f9fd / I-207, witness-form, AS
//   PRE-REGISTERED): noHotpathReclaim HOLDS under calibStep
//   (268,869/46,623 — the repair path is gone) while boundsOK,
//   m023NonNegative, CR-1, CR-2, CR-3, CR-4 and S5 all HOLD over the
//   same alphabet — the revert's harm is latency, not safety. The with-
//   mechanism half is the wired Stage-B witness quint-chunk-liveness-
//   witness-hotpath-reclaim.
// - ENC-A by analogy: 064ceadbd covered by refcountCalibG5NoHeartbeat
//   (the model does not distinguish progress-driven from wall-clock
//   heartbeats); its inline-ingest plumbing is outside the chunked-
//   upload scope.
module refcountCalibG5NoHeartbeat {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

  /// PRE-FIX clock: time passes with no heartbeat contract — a live,
  /// guard-armed owner's placeholder ages toward the reclaim thresholds
  /// exactly like an abandoned one. Same effects as the as-built tick.
  action tickNoHeartbeat: bool = all {
    paths' = PATH_KEYS.mapBy(p =>
      val r = paths.get(p)
      if (r.status == PUploading) { ...r, staleness: minInt(r.staleness + 1, STALE_CAP) } else r),
    chunks' = HASHES.mapBy(h =>
      val c = chunks.get(h)
      if (c.present) { ...c, age: minInt(c.age + 1, AGE_CAP) } else c),
    s3' = s3, outbox' = outbox, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs, wit' = wit,
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
      tickNoHeartbeat,
    }
  }
}

// =========================================================================
// CALIBRATION — pre-2d7e4f9fd (I-207): no hot-path stale reclaim — a
// stale 'uploading' placeholder blocks every re-claim of its path until
// the 15-minute orphan scanner gets to it (the historical harm:
// substitution of a path whose previous upload had crashed stalled for
// up to 15 minutes). The revert removes the hot-path reclaim from the
// alphabet; nothing else changes.
// PREDICTED (witness form, per the design's pre-registration): the
// `noHotpathReclaim` witness is NOT violable under calibStep (the
// repair path is gone — quint reports "No violation found", which for a
// witness means the probed scenario is unreachable), while the
// committed safety invariants still HOLD over the same alphabet (the
// harm is latency, not safety). The as-built half of the pair — the
// witness IS violated when the mechanism exists — is the wired Stage-B
// check quint-chunk-liveness-witness-hotpath-reclaim.
// =========================================================================
module refcountCalibG5NoHotpathReclaim {
  import chunkLiveness(
    MAX_HASHES = 1,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = true,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).* from "../chunkLiveness"

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
