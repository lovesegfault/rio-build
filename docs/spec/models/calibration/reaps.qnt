// -*- mode: Bluespec; -*-
//
// Calibration family: the orphan reaps
// (6aab70b47 / 463090eb7 / f8ce10b8e / 81824cfbb).
//
// The family's question for Phase 6: which of the four reaps are
// individually load-bearing for everyRetainedEntryIsJustified? Answer
// (machine-checked by the modules below): the two PERIODIC reaps
// (refused-UPSERT, sealed-empty) are — each has an orphan shape no
// other mechanism covers. The two EVENT-COUPLED reaps (enqueue-failure
// here, tenure-drop in the tenure-pins family) are latency
// optimizations of the periodic pair: the safety invariant cannot see
// the latency, so they verify as redundant — but their code-level
// justifications (the dead-flusher mode in which the periodic reaps
// never run again; the GetDerivationLogs shadow window) are
// NOT-ENCODED, so redundant-in-the-model is not deletable-from-the-code.
//
// Every reap-disable module gates the reap action, its reapEnabledFor
// disjunct, AND its half of the pre-recovery-window justification
// disjunct on the same switch (see the const block's comment in the
// main model): an invariant that kept promising "this reap will fire"
// about a reap that does not exist in the instantiated module would
// make every override below trivially HOLD.

// =========================================================================
// CALIBRATION — pre-6aab70b47: the terminal epilogue does not reap the
// zero-line entry whose final FlushRequest could not be enqueued.
// Fault-local regime (the one where ENABLE_ENQUEUE_FAILURE is on) with
// the enqueue-failure reap disabled.
//
// PREDICTED HOLDS: the orphan this reap targets (empty, sealed,
// not final-pending, DagTerminal) is covered by the terminal-cleanup
// discard from the moment it is created and by the periodic
// sealed-empty reap on every tick of an authoritative leader. The reap
// is NOT individually load-bearing for the safety invariant; its
// code-level value is the dead-flusher mode and the read-path shadow
// latency, both outside the model.
// =========================================================================
module logBufferLifecycleNoEnqueueFailReap {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 3,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = true,
    ENABLE_LOAD_FAILURE = false,
    ENABLE_DISCARDED_RECOVERY = false,
    ENABLE_ENQUEUE_FAILURE = true,
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
    ENABLE_REAP_ENQUEUE_FAIL = false,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = false,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — pre-463090eb7 / pre-f8ce10b8e: no reaper exists for a
// sealed non-empty entry whose execution another tenure finalized. The
// fault-persist regime (the only one in which that shape is reachable:
// interimFinalizes needs a live assignment behind our seal, which only
// the terminal-persist failure leaves in place) with the periodic
// refused-UPSERT reap disabled.
//
// everyRetainedEntryIsJustified MUST be violated: the orphan is
// excluded from the best-data-available disjunct by the finalized row,
// excluded from the live-carrier and pending-final disjuncts by its
// history, and — with the reap and its reapEnabledFor promise both
// off — excluded from the one-step-from-reaped disjunct. No other reap
// rescues it: the reconcile never empties a finalized row's ring (so
// the sealed-empty reap stays disabled), terminal cleanup is blocked by
// the final-pending mark, the sweep and the disconnect discard skip
// sealed keys, and the tenure-drop reap already had its one chance and
// refused because the ring was non-empty.
//
// One module, two corpus rows: 463090eb7 (the no-reap-at-all bug its
// message describes) and f8ce10b8e variant A (the surviving mechanism).
// Wired as quint-log-calib-no-refused-upsert-reap in nix/quint.nix.
// =========================================================================
module logBufferLifecycleNoRefusedUpsertReap {
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
    ENABLE_REAP_REFUSED_UPSERT = false,
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
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — pre-f8ce10b8e exactly: 463090eb7's one-shot drv_logs
// consult at the tenure-drop arm is the ONLY reaper of the
// finalized-elsewhere orphan. everyRetainedEntryIsJustified MUST still
// be violated, through the ordering the f8ce10b8e commit message names:
// the request is dropped (and the consult finds the row not yet
// finalized) BEFORE the interim leader finalizes the execution. The
// consult never re-runs, so the orphan is permanent. A red check here
// plus a green quint-log-fault-persist is the machine-checked statement
// that moving the reap to the recurring periodic chokepoint was a
// necessary fix, not a performance refactor. (The other half of
// f8ce10b8e's motivation — the drop arm serially burning pool-acquire
// timeouts on a backlog of orphaned finals — is a liveness/latency
// property the model does not encode.)
// =========================================================================
module logBufferLifecycleOneShotConsult {
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
    ENABLE_REAP_REFUSED_UPSERT = false,
    ENABLE_REAP_SEALED_EMPTY = true,
    ENABLE_REAP_ENQUEUE_FAIL = true,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = true,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — pre-81824cfbb: the periodic flush's empty-snapshot early
// return does not reap the sealed empty entry. The flap regime with the
// sealed-empty reap disabled (and its reapEnabledFor /
// pre-recovery-window promises disabled with it).
//
// everyRetainedEntryIsJustified MUST be violated by the shape the fix's
// commit message names: a sealed non-empty orphan kept by the
// out-of-tenure drop arm whose ring the stored-coverage reconcile then
// empties (a non-finalized interim-leader row covering past the
// retained tail). Once empty it leaves the best-data-available
// disjunct; once the request is consumed it leaves the pending-final
// disjunct; the tenure-drop reap already had its one chance while the
// ring was still non-empty; the refused-UPSERT reap needs a non-empty
// ring; the sweep and the disconnect discard skip sealed keys; cleanup
// skips final-pending entries. This is the same depth-13 state the
// phase-2 rebound finding reduced through — minus the rebound, plus the
// reap disabled. The deepest counterexample in the calibration set;
// random simulation does not find it (the phase-2 finding's 20k
// 24-step traces never reached the adjacent state).
//
// Wired as quint-log-calib-no-sealed-empty-reap in nix/quint.nix.
// =========================================================================
module logBufferLifecycleNoSealedEmptyReap {
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
    ENABLE_REAP_SEALED_EMPTY = false,
    ENABLE_REAP_ENQUEUE_FAIL = true,
    ENABLE_DROP_ARM_FINALIZED_CONSULT = false,
    ENABLE_BINDING_GATE = true,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}
