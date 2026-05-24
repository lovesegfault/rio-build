// -*- mode: Bluespec; -*-
//
// Calibration family: the cross-exec / same-exec restamp clears
// (6c26e85f8 / 699ea1692 / 2f9a747d0) — what `LogBuffers::set_exec`
// stops carrying across a recovery-time restamp of a retained entry.
//
// The family's headline for the model: the line-clear reversion is the
// one calibration entry in the corpus that falsifies an ORIGINAL
// model-A invariant (noCrossExecContamination); the two seal-clear
// reversions falsify nothing in the original five — the model sees
// MISATTRIBUTION (one execution's data under another's key) but is
// structurally blind to MUTING (a live execution's data refused at the
// gate), because the conservation law deliberately quantifies over
// accepted lines only. noStaleSealOnLiveCarrier (added in phase 3) is
// the cheap repair: one safety invariant over existing state
// (sealed x hasEntry x dag), not a new state dimension.

// =========================================================================
// CALIBRATION — a cross-exec restamp carries the prior execution's lines
// (the pre-6c26e85f8 set_exec, which overwrote only exec_id and
// assigned_executor and relied on every caller to discard() first;
// recovery's restamp does not). After an A→B→A flap in which the interim
// re-dispatched the drv under a fresh exec_id, the ex-leader's retained
// entry — still holding execution 1's unflushed tail — is stamped with
// execution 2's exec_id and the flusher uploads execution 1's lines
// under execution 2's S3 key and drv_logs row.
//
// Flap regime. noCrossExecContamination MUST be violated at the
// post-recoveryOk state itself (the restamped entry holds lines the
// ingestion gate attributed to execution 1 while stamped for execution
// 2); no flush needs to run. Reachability of the precondition (a
// non-empty ring at a cross-exec restamp) is independently pinned by
// quint-log-witness-cross-exec-clear in the same regime.
//
// Wired as quint-log-calib-cross-exec-carries-lines in nix/quint.nix.
// =========================================================================
module logBufferLifecycleCrossExecCarriesLines {
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
    ENABLE_CROSS_EXEC_LINE_CLEAR = false,
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

  /// The minimal pre-fix counterexample: an ex-leader's retained line is
  /// restamped into the interim leader's re-dispatched execution.
  run crossExecCarriesLinesFalsifiesContaminationRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 1))
      .then(loseLease)
      .then(foreignSteal)
      .then(interimRedispatches("d1", "e2"))
      .then(reacquireAfterHolderChange)
      .then(recoveryOk)
      .expect(all {
        entryExec("d1") == 2,
        entryHeld("d1") == Set(0),
        delivered.get("d1").get(2) == Set(),
        not(noCrossExecContamination),
      })
}

// =========================================================================
// CALIBRATION — a cross-exec restamp keeps the prior execution's seal
// (the pre-699ea1692 set_exec). Fault-persist regime — the only one in
// which a sealed entry can meet a cross-exec restamp: a terminal whose
// PG persist fails leaves the assignment live, an interim leader
// re-dispatches it under a fresh exec, and the re-acquiring leader
// restamps the still-sealed entry. push_for checks the seal before the
// binding gate, so every batch of the re-dispatched execution is
// silently dropped.
//
// VERDICT against the original five invariants: HOLDS — the bug's only
// consequence is that the new execution's batches are REJECTED, and
// every original invariant quantifies over gate-ACCEPTED lines,
// retained entries, or written rows. noStaleSealOnLiveCarrier (phase 3)
// is the invariant that observes it: the restamped entry is sealed
// while its derivation is DagLive.
// =========================================================================
module logBufferLifecycleCrossExecKeepsSeal {
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
    ENABLE_CROSS_EXEC_UNSEAL = false,
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
// CALIBRATION — a same-exec restamp keeps the seal and the final-pending
// mark (the pre-2f9a747d0 set_exec). Fault-persist regime: the sealed
// entry meets a same-exec restamp only when the terminal's PG persist
// failed and no interim re-dispatch happened. The re-acquired leader's
// reconnected worker keeps streaming, but every post-flap batch is
// rejected by the retained seal; the prior tenure's retained final is
// tenure-dropped without unsealing and no other path unseals a sealed
// entry; the retained mark exempts the entry from terminal cleanup
// forever.
//
// VERDICT against the original five invariants: HOLDS — same structural
// blindness as the cross-exec seal (the muting is invisible to the
// conservation law); the mark half is additionally absorbed by
// everyRetainedEntryIsJustified's best-data-available disjunct (the
// periodic flush genuinely does keep snapshotting the orphan).
// noStaleSealOnLiveCarrier (phase 3) observes the seal half.
// =========================================================================
module logBufferLifecycleSameExecKeepsSeal {
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
    ENABLE_SAME_EXEC_RESTAMP_CLEAR = false,
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
