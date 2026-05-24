// -*- mode: Bluespec; -*-
//
// Calibration family: line-number-space accounting
// (d8944727e / effefb0a1 / e1fd179b9) — the fixes that keep the
// recorded (first_line, line_count) of a drv_logs row honest when the
// flush payload is not a plain contiguous ring.
//
// The two span-arithmetic rows are the UNDERSHOOT duals of calibration
// entry #0 (the Ungated module's overshoot): an understated recorded
// end shrinks the accounted region below delivered lines, so both
// falsify noSilentLineLoss as well as lineSpanExact once the row is
// finalized. The fold-trigger row (e1fd179b9) is a coverage-regression
// bug, not a span-arithmetic bug: the overwriting row is internally
// consistent, the harm is that its end regresses below what the interim
// leader had recorded — observable only by the phase-3
// storedCoverageNeverRegresses ghost high-water.

// =========================================================================
// CALIBRATION — the gap-merge fold counts the synthetic marker line
// instead of the gap (pre-d8944727e): the marker stands in for `gap`
// lost lines but is counted as 1, so first_line + line_count
// understates the execution's true end by gap - 1 whenever gap >= 2.
// s3_is_caught_up reports caught-up while the stored tail is unread,
// and the next failover's lookup_stored_prefix computes its gap from
// the understated end and folds a spurious marker per flap
// (compounding).
//
// Flap-regime action set with MAX_LINE = 4: the override only diverges
// at gap >= 2 (a cached prefix ending at P and a ring starting at
// >= P + 2), and the shipped MAX_LINE = 3 caps the reachable gap at 1,
// where marker(1) == gap(1) and the pre-fix arithmetic is accidentally
// exact. That bound sensitivity is itself a finding: the shipped flap
// regime's green lineSpanExact verdict over the gap-merge fold covers
// only the gap <= 1 cases where the marker-vs-gap distinction is
// invisible. lineSpanExact and noSilentLineLoss MUST be violated.
// MAX_LINE = 4 roughly doubles every per-exec line powerset; this is a
// one-shot calibration run, not a CI candidate.
// =========================================================================
module logBufferLifecycleMarkerCountedSpan {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 4,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = false,
    ENABLE_PERSIST_FAILURE = false,
    ENABLE_GUARD_FAILURE = false,
    ENABLE_ACCOUNTED_FLOOR = true,
    ENABLE_TENURE_PIN = true,
    ENABLE_TENURE_PIN_FIRST = true,
    ENABLE_CROSS_EXEC_LINE_CLEAR = true,
    ENABLE_CROSS_EXEC_UNSEAL = true,
    ENABLE_SAME_EXEC_RESTAMP_CLEAR = true,
    ENABLE_REAP_REFUSED_UPSERT = true,
    ENABLE_REAP_SEALED_EMPTY = true,
    ENABLE_REAP_ENQUEUE_FAIL = true,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = false,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = false,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — a flush payload's span contribution is its physical
// line count instead of its line-number span (pre-effefb0a1): a
// re-acquired ex-leader's retained ring can carry an interior hole
// (lines delivered only to an interim leader that never flushed them),
// so the row understates the execution's end by the hole size;
// s3_is_caught_up then short-circuits cursors below the true end, and
// the read path's physical-vs-claimed re-serve cannot fire because the
// counts match.
//
// Base regime, existing constants — the cheapest counterexample in the
// whole calibration corpus: the model reaches an interior hole without
// a failover because the accept gate admits any batch starting at or
// above the accounted floor (an upward gap becomes an interior hole),
// which is the same trust-boundary behavior the real push_for has.
// lineSpanExact MUST be violated at the first flush of a holey ring;
// noSilentLineLoss at its finalization. This is the base regime's
// interiorHoleRun with the override applied.
//
// Wired as quint-log-calib-physical-count-span in nix/quint.nix — the
// regression it guards against (someone "simplifying" ring_span back to
// a count) is the most plausible in the family.
// =========================================================================
module logBufferLifecyclePhysicalCountSpan {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 3,
    MAX_TRANSITIONS = 0,
    MAX_GEN = 3,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = false,
    ENABLE_PERSIST_FAILURE = false,
    ENABLE_GUARD_FAILURE = false,
    ENABLE_ACCOUNTED_FLOOR = true,
    ENABLE_TENURE_PIN = true,
    ENABLE_TENURE_PIN_FIRST = true,
    ENABLE_CROSS_EXEC_LINE_CLEAR = true,
    ENABLE_CROSS_EXEC_UNSEAL = true,
    ENABLE_SAME_EXEC_RESTAMP_CLEAR = true,
    ENABLE_REAP_REFUSED_UPSERT = true,
    ENABLE_REAP_SEALED_EMPTY = true,
    ENABLE_REAP_ENQUEUE_FAIL = true,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = false,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = false,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"

  /// interiorHoleRun with the override applied: the worker skips line 1,
  /// the recorded count is the physical |{0,2}| = 2 instead of the
  /// line-number span 3, and the finalized row both misstates its span
  /// and fails to cover delivered line 2.
  run physicalCountUnderstatesHoleyRingRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 1))
      .then(pushBatchAccepted("d1", "e1", 2, 3))
      .then(terminalEnqueued("d1"))
      .then(reconcileStoredPrefix("d1"))
      .then(flushFinalUpload("d1"))
      .expect(all {
        rowCnt("d1", 1) == 2,
        rowComplete("d1", 1),
        not(lineSpanExact),
        not(noSilentLineLoss),
      })
}

// =========================================================================
// CALIBRATION — the stored-coverage reconcile folds only disjoint rows
// (pre-e1fd179b9): a stored row is declared "not needed" whenever it
// overlaps the ring at all, so a re-acquired ex-leader whose retained
// ring still starts at line 0 overwrites the interim leader's .partial
// (which may contain lines this replica never received) with its own
// shorter snapshot, and the final flush deletes the only remaining
// copy.
//
// Flap regime. The original five invariants HOLD: the overwriting row
// is internally consistent (lineSpanExact), and the destroyed lines
// were delivered only to the interim leader — outside the per-replica
// delivered ghost by design (noSilentLineLoss). The phase-3
// storedCoverageNeverRegresses ghost high-water is the invariant that
// observes it: the row's recorded end regresses from the interim's
// extension back to this replica's ring end.
// =========================================================================
module logBufferLifecycleDisjointFoldOnly {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 3,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = false,
    ENABLE_PERSIST_FAILURE = false,
    ENABLE_GUARD_FAILURE = false,
    ENABLE_ACCOUNTED_FLOOR = true,
    ENABLE_TENURE_PIN = true,
    ENABLE_TENURE_PIN_FIRST = true,
    ENABLE_CROSS_EXEC_LINE_CLEAR = true,
    ENABLE_CROSS_EXEC_UNSEAL = true,
    ENABLE_SAME_EXEC_RESTAMP_CLEAR = true,
    ENABLE_REAP_REFUSED_UPSERT = true,
    ENABLE_REAP_SEALED_EMPTY = true,
    ENABLE_REAP_ENQUEUE_FAIL = true,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = false,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = false
  ).* from "../logBufferLifecycle"
}
