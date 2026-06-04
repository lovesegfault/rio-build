// -*- mode: Bluespec; -*-
//
// The tail reader loop: re-open pacing and the orphan exit
// (bughunt-2 wave, merged_bug_130 / merged_bug_054; spec rules
// store.log.tail-grace-drain, dash.stream.reopen-pacing).
//
// WHAT IS MODELED
//   One reader (the gateway relay's run_tail, or the dashboard's
//   follow loop — same decision skeleton) cycling stream attempts
//   against a store. Each re-open yields a productive chunk (serve /
//   gapThenServe), a bare receipt (the store's always-final arm: a
//   zero-line keep-alive or fully-resent chunk — the `skip` verdict),
//   or fails at the open. The consumer set can die at any point
//   (orphan: the drain sender vanishes); the kernel law maps Orphaned
//   to Exit unconditionally.
//
// THE PROPERTIES (state invariants):
//   orphanNeverReopens (130) — once the orphan signal is observed, the
//     open count is frozen at its orphan-time checkpoint: an orphaned
//     relay never opens another stream. The shipped defect mapped the
//     dead-drain-sender error to naturalEnd, which re-opened at zero
//     backoff (~8,500 opens/s observed).
//   pacingEscalatesAbsentProgress (054) — the pacer level always equals
//     min(cycles-since-progress, cap): receipt without progress (skip)
//     escalates exactly like a failed open and only productive
//     verdicts reset. The shipped defect reset on ANY receipt, so the
//     always-final arm held the loop at the base delay forever.
//
// CALIBRATIONS (expect-violation):
//   tailreader-orphan-hotloop (130)   — the orphan cause keeps the
//                                       reopen arm enabled (the shipped
//                                       closed→naturalEnd mapping).
//   tailreader-reset-on-receipt (054) — a bare receipt resets the pacer
//                                       (the shipped
//                                       receivedThisAttempt reset).
//
// Tier-1 budget: reopens <= MAX_REOPENS bounds everything; hundreds of
// states, TLC exhausts instantly.

module tailReaderLoop {
  /// Total re-open attempts the model explores (state-space bound, not
  /// a semantic cap — the semantic caps under test are the pacer level
  /// and the orphan freeze).
  val MAX_REOPENS: int = 5
  /// Pacer ladder cap: level 0..3 == 250/500/1000/2000 ms.
  val PACER_MAX_LEVEL: int = 3

  /// The drain sender vanished: no consumer remains.
  var orphanSignal: bool
  /// The reader exited (the law's only legal continuation once
  /// orphaned).
  var exited: bool
  /// Stream opens performed.
  var reopens: int
  /// Checkpoint of `reopens` taken when the orphan signal lands.
  var reopensAtOrphan: int
  /// The READER's pacer ladder level (the implementation artifact
  /// under test).
  var pacerLevel: int
  /// ORACLE SEAT: re-open cycles since the last productive verdict,
  /// written by the same live actions; the invariant pins pacerLevel
  /// to min(sinceProgress, cap).
  var sinceProgress: int

  pure def min2(a: int, b: int): int = if (a < b) a else b

  action init = all {
    orphanSignal' = false,
    exited' = false,
    reopens' = 0,
    reopensAtOrphan' = 0,
    pacerLevel' = 0,
    sinceProgress' = 0,
  }

  /// A re-open whose stream yields a productive verdict (serve or
  /// gapThenServe): the ladder resets — progress is the only reset key.
  action reopenServe = all {
    not(orphanSignal), not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = 0,
    sinceProgress' = 0,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  /// A re-open whose stream yields only bare receipts (the store's
  /// always-final arm: zero-line keep-alive / fully-resent chunk — the
  /// `skip` verdict). RECEIPT IS NOT PROGRESS: escalates exactly like
  /// a failed open.
  action reopenSkip = all {
    not(orphanSignal), not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = min2(pacerLevel + 1, PACER_MAX_LEVEL),
    sinceProgress' = sinceProgress + 1,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  /// A re-open that fails at the open itself: escalates.
  action reopenFailed = all {
    not(orphanSignal), not(exited), reopens < MAX_REOPENS,
    reopens' = reopens + 1,
    pacerLevel' = min2(pacerLevel + 1, PACER_MAX_LEVEL),
    sinceProgress' = sinceProgress + 1,
    orphanSignal' = orphanSignal, exited' = exited,
    reopensAtOrphan' = reopensAtOrphan,
  }

  /// The consumer set dies: the drain sender vanishes. The kernel law
  /// (tail_next(Orphaned, ..) = Exit, proven by
  /// check_tail_next_orphan_always_exits) forbids any further open —
  /// the open count freezes at its checkpoint.
  action orphanHappens = all {
    not(orphanSignal), not(exited),
    orphanSignal' = true,
    reopensAtOrphan' = reopens,
    exited' = exited, reopens' = reopens,
    pacerLevel' = pacerLevel, sinceProgress' = sinceProgress,
  }

  /// The orphaned reader exits — its only legal continuation.
  action exitLoop = all {
    orphanSignal, not(exited),
    exited' = true,
    orphanSignal' = orphanSignal, reopens' = reopens,
    reopensAtOrphan' = reopensAtOrphan,
    pacerLevel' = pacerLevel, sinceProgress' = sinceProgress,
  }

  action step = any {
    reopenServe,
    reopenSkip,
    reopenFailed,
    orphanHappens,
    exitLoop,
  }

  // ---- invariants ----

  // merged_bug_130: an orphaned reader never opens another stream.
  val orphanNeverReopens =
    orphanSignal implies (reopens == reopensAtOrphan)

  // merged_bug_054: the pacer ladder is honest — its level always
  // equals the oracle's cycles-since-progress (capped), so a reset on
  // a bare receipt diverges on the first skip cycle.
  val pacingEscalatesAbsentProgress =
    pacerLevel == min2(sinceProgress, PACER_MAX_LEVEL)

  val tailReaderLoopInvariants = all {
    orphanNeverReopens,
    pacingEscalatesAbsentProgress,
  }
}
