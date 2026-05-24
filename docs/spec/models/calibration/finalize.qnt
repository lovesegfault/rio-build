// -*- mode: Bluespec; -*-
//
// Calibration family: finalize-once (abfade4d5 / 44905298b).
//
// Both modules MUST keep ENABLE_PERSIST_FAILURE = true: the unsafe
// write's target state (a finalized row coexisting with a live
// retained entry for the same exec) requires the terminal-persist
// failure — every other terminal path clears the assignment, and
// interimFinalizes clears it too, so no later recovery can rebuild a
// live carrier for a finalized exec without it. A variant cloned from
// the flap regime's constants would HOLD vacuously and produce a false
// "fix is redundant" signal.
//
// Both overrides falsify finalizedRowFrozen (phase 3) at the overwrite
// step itself AND noSilentLineLoss a few steps later (the downgrade /
// re-finalization regresses the recorded span below lines this replica
// delivered under the first finalization of the same exec across two
// tenures). The headline cross-replica harms (IsCompleteMonotone,
// AtMostOneFinalizingWrite, the blob/row split across the guard
// SELECT -> PUT window) remain model B's.

// =========================================================================
// CALIBRATION — the periodic UPSERT has no frozen-row latch
// (pre-abfade4d5): the DO UPDATE has no WHERE clause, so a periodic
// .partial snapshot overwrites every content column of a finalized row
// AND downgrades is_complete true -> false. Nothing ever repairs the
// row (the finalized buffer is drained — there is no next snapshot).
//
// Fault-persist regime. finalizedRowFrozen MUST be violated at the
// downgrade step itself; noSilentLineLoss MUST be violated a few steps
// later (the downgrade un-freezes the row, a later legitimate final
// re-finalizes it with the post-recovery ring's smaller span, and the
// lines delivered under the first finalization are no longer covered).
// The motivating interleaving (the mid-sweep is_leader() staleness) is
// NOT-ENCODED; the model reaches the same write through the
// A->B->A retained-entry route.
// =========================================================================
module logBufferLifecycleNoPeriodicLatch {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 2,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = false,
    ENABLE_PERSIST_FAILURE = true,
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
    ENABLE_PERIODIC_FROZEN_ROW = false,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — flush_final has no already-finalized consult
// (pre-44905298b): a second "final" for an already-finalized exec
// drains the retained residue and overwrites the finalized row (and the
// .log.zst blob) with the residue's smaller span while keeping
// is_complete = true — silently destroying the only durable copy of the
// first finalization's content.
//
// Fault-persist regime. finalizedRowFrozen MUST be violated at the
// re-finalizing write; noSilentLineLoss MUST be violated on the same
// trace (the second finalization's recorded span is the post-recovery
// ring's, which ends below lines this replica delivered under the
// first tenure of the same exec — delivered accumulates across tenures,
// the first finalization covered all of it, the second regresses below
// it). The model reaches the guard through the single-replica
// self-re-finalization route (terminalPersistFailed leaves the
// assignment live across our own finalization), not the commit
// message's A->B->A reset-out-of-terminal route (the model has no
// reset-to-ready transition — NOT-ENCODED dimension N4).
//
// Wired as quint-log-calib-no-finalize-refusal in nix/quint.nix.
// =========================================================================
module logBufferLifecycleNoFinalizeRefusal {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 2,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = false,
    ENABLE_PERSIST_FAILURE = true,
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
    ENABLE_FINALIZE_REFUSAL = false,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}
