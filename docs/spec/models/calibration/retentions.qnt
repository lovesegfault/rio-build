// -*- mode: Bluespec; -*-
//
// Calibration family: deposed-leader retentions and re-acquisition
// sweeps (2c301438d / e6c18add2 / 8e97c2220).
//
// The lease-flap retention design pulls in two directions: don't
// discard too eagerly while the DAG is not authoritative (2c301438d —
// the pre-fix code LOST LINES), and do discard at the next acquisition
// everything the rebuilt DAG no longer justifies (e6c18add2 /
// 8e97c2220 — the pre-fix code LEAKED ENTRIES). The third commit's
// delta (poisoned drvs spared by the sweep) is NOT-ENCODED: the model
// has no persisted-terminal-but-DAG-resident state, so the pre- and
// post-fix sweep predicates are extensionally equal at its granularity.

// =========================================================================
// CALIBRATION — the disconnect-time log-buffer discard runs
// unconditionally (pre-2c301438d): "not in the DAG" is read as "stale"
// even on a deposed leader whose DAG was just wiped, so the worker's
// stream teardown (which lands seconds after the lease flip) destroys
// exactly the retained, unsealed, in-flight entries the lease-flap
// retention preserves. The next acquisition's same-exec restamp lands
// on an empty entry with the accounted floor reset, the reconnecting
// worker only re-streams un-acked batches, and the final blob is
// uploaded with a hole as is_complete = true.
//
// Flap regime. noSilentLineLoss MUST be violated: the discarded lines
// were never stored, are not counted in the finalized record's claimed
// span, and are not disclosed by an incomplete indicator. lineSpanExact
// stays green on the same trace (the row is exact about the span it
// claims; it just claims too little) — the clean separation between the
// span-arithmetic invariant and the conservation law.
// =========================================================================
module logBufferLifecycleDisconnectUngated {
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
    ENABLE_DISCONNECT_DAG_GATE = false,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — there is no acquisition-time sweep (pre-e6c18add2):
// recovery only restamps entries for drvs still Assigned/Running in PG;
// an entry for a drv that went terminal under the interim leader is
// never restamped or discarded by any path — its pre-flap lines shadow
// the execution's stored log in GetDerivationLogs indefinitely and
// flush_periodic re-uploads its .partial every 30s for the process
// lifetime.
//
// Flap regime. everyRetainedEntryIsJustified MUST be violated
// immediately after the recovery that should have swept the entry: the
// un-swept entry is not DagLive, has no pending final, sits on an
// authoritative recovered leader, and (its row having been finalized by
// the interim) is excluded from the best-data-available disjunct.
// LOAD-BEARING DEPENDENCY: this falsification only exists because
// phase-3 commit 1 removed the disconnect-discard disjunct from
// reapEnabledFor — with that environment-initiated reap still counted
// as a justification, the un-swept orphan is declared "one step from
// reaped" forever by a reap that only fires if the worker's stream
// happens to close. The dossier's predicted HOLDS-as-written is
// resolved by that invariant tightening, not by an override change.
// =========================================================================
module logBufferLifecycleSweepless {
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
    ENABLE_ACQUISITION_SWEEP = false,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}
