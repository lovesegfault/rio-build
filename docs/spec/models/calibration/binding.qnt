// -*- mode: Bluespec; -*-
//
// Calibration family: the (executor, drv) binding gates
// (496e6fb14 / 7beb1ca00 #1 / 7ffbf1415).
//
// Of the three consumers of a worker-supplied derivation_path the family
// gates, the model represents exactly one (the ring write); the gateway
// log fan-out and the BuildPhase fan-out are display-only sinks with no
// model footprint (NOT-ENCODED — see the calibration table). The one
// encodable override is the accept predicate's executor conjunct.

// =========================================================================
// THE UNBOUND VARIANT — calibration entry for 496e6fb14 (wire the
// (executor, drv) binding check into the recv task).
//
// The base regime with ENABLE_BINDING_GATE = false: the batch accept
// predicate stops comparing the calling executor against the entry's
// binding — the pre-fix recv task, which appended any unsealed batch via
// the legacy push(). bindingGateExcludesForeignExecutors is VIOLATED one
// step from init (after dispatch(d1, e1) the predicate admits
// (d1, e2, 0) while e1 is the bound executor).
//
// The other invariants HOLD — provably, not just empirically: the `ex`
// parameter of pushBatchAccepted appears only in the guard, so for any
// state the ungated predicate admits a foreign executor's batch into,
// the bound executor could have pushed the identical batch and produced
// the identical successor. Ungating changes which guard evaluations are
// true, not which states are reachable. The real-world harm (another
// worker's BYTES under this execution's blob and banner) lives in the
// content/provenance dimension the Set[int] line encoding erases; the
// invariant that does falsify is the gate's own definition, and its
// value is non-vacuity of the quantifier (a refactor that weakens
// batchAccepted goes red here instead of silently making the regime
// checks vacuous).
//
// Wired as quint-log-calib-binding-ungated in nix/quint.nix (the
// cheapest witness in the calibration set).
// =========================================================================
module logBufferLifecycleUnbound {
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
    ENABLE_BINDING_GATE = false,
    ENABLE_DISCONNECT_DAG_GATE = true,
    ENABLE_ACQUISITION_SWEEP = true,
    ENABLE_PERIODIC_FROZEN_ROW = true,
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).* from "../logBufferLifecycle"

  /// The minimal pre-fix counterexample: one dispatch is enough. The
  /// state after dispatch already satisfies batchAccepted("d1","e2",0)
  /// (unsealed, entry exists, executor conjunct disabled, 0 >= floor 0)
  /// while entryExecutor("d1") == "e1".
  run unboundForeignPushAdmittedRun =
    init
      .then(dispatch("d1", "e1"))
      .expect(all {
        batchAccepted("d1", "e2", 0),
        entryExecutor("d1") == "e1",
        not(bindingGateExcludesForeignExecutors),
      })
}
