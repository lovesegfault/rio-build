// -*- mode: Bluespec; -*-
//
// Calibration family: the deferred-final tenure pins
// (84ac79b84 / 3ce5d03e4 / 646022e37 / 825cdd478).
//
// The family's headline: NONE of its four fixes is re-findable by a
// model-A state invariant. Two rows (84ac79b84, 825cdd478) are
// NOT-ENCODED — their pre-fix behavior is an intra-action interleaving
// at lock or await granularity that the model's action atomicity
// collapses. The two encodable rows (below) revert guards that ARE
// model state, and both HOLD on every invariant including the phase-3
// additions: the harm of an unpinned or late-pinned final is either
// (a) the freeze/regression of a drv_logs row another replica is
// extending — model B's TenurePinnedFinalization — or (b) the
// destruction of the live execution's restamped carrier, which is a
// TRANSITION property (which request did the draining) whose post-state
// is identical to the legitimate in-tenure drain's (see the
// liveDerivationHasCarrier rejection in log-invariant-map.md). The
// machine-checked HOLDS below is the calibration datum: model A alone
// cannot justify the tenure-pin family; the rio-scheduler deferred-final
// unit tests are the only automated check standing between these fixes
// and a regression until model B exists.

// =========================================================================
// CALIBRATION — the tenure pin does not exist (pre-3ce5d03e4): a final
// flush request is processed regardless of leadership — first attempt
// and deferred retry alike — on the premise that the build behind it
// has already ended. The flap regime's lease budget with the
// terminal-persist fault (the correlated failure: a request only sits
// deferred when PG is unreachable, which is also when the terminal
// persist fails and the lease lapses, so the next recovery restamps the
// SAME exec as the live carrier and the stale final then drains and
// freezes it). MAX_LINE = 2 (the fault-persist regime's bound — the
// subject is the entry/seal/request lifecycle, not line arithmetic; the
// dossier's draft used 3, which puts the persist subtree several times
// past the per-check budget for no additional interleaving class).
//
// EXPECTED VERDICT: all nine invariants HOLD. A green run here is the
// machine-checked statement "model A alone cannot re-find the
// tenure-pin bug"; the falsification belongs to Phase 4's model B
// (TenurePinnedFinalization / StoredCoverageNeverRegresses in their
// cross-replica forms). Not wired into CI; the table is the record.
// =========================================================================
module logBufferLifecycleTenureUnpinned {
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
    ENABLE_TENURE_PIN = false,
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
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}

// =========================================================================
// CALIBRATION — the tenure pin sits AFTER the already-finalized guard
// (pre-646022e37, post-3ce5d03e4): the refusal arms' residue
// drain/reap/retention run for out-of-tenure requests too (exec-guarded
// only), and the drop arm's orphan reap does not exist yet. The
// persist x guard product the shipped regime split deliberately
// excludes — the persist fault makes recovery's same-exec restamp adopt
// the orphaned entry as the live carrier, the guard fault makes the Err
// arm's destructive reap reachable, and the lease budget makes the
// request out of tenure.
//
// EXPECTED VERDICT: all nine invariants HOLD — the harm (an
// out-of-tenure request's guard-error arm reaps the live execution's
// restamped carrier; every later push_for rejects) is a transition
// property whose post-state is identical to the legitimate in-tenure
// reap's. The third destructive arm this commit gated (the
// cap-overflow drain) is unmodeled (the model has one pending-final
// slot per drv, no deferred-queue cap). NOTE: this module's constants
// are the persist x guard product; expect a state count well above the
// per-check budget. One-shot calibration evidence only — never promote
// to CI without re-bounding.
// =========================================================================
module logBufferLifecyclePinAfterGuard {
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
    ENABLE_GUARD_FAILURE = true,
    ENABLE_ACCOUNTED_FLOOR = true,
    ENABLE_TENURE_PIN = true,
    ENABLE_TENURE_PIN_FIRST = false,
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
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"
}
