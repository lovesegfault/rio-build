// -*- mode: Bluespec; -*-
//
// Model A of the rio-scheduler build-log subsystem: the single-replica
// log-buffer entry lifecycle (`LogBuffers` + `LogFlusher` in
// rio-scheduler/src/logs/{mod,flush}.rs and the actor paths that drive
// them), driven by an adversarial environment of worker batches, lease
// transitions, recovery runs, flusher ticks, terminals, and reaps.
//
// WHAT IS MODELED (the protocol inventory's P1 + P2's binding gate + P4's
// effect on the local ring; see docs/spec/models/log-invariant-map.md for
// the invariant ↔ rule mapping this file implements the model-A half of):
//   - the per-derivation ring-buffer entry: execution stamp, executor
//     binding, held lines, seal tombstone, pending-final mark,
//     stored-coverage reconciliation state;
//   - the batch accept/reject predicate (seal, binding gate, and the
//     line-numbering floor: one past the highest line the entry has ever
//     accounted for, including the cached stored prefix's end);
//   - the byte-cap eviction, push-coupled exactly as the real one is: it
//     fires only inside an accepting push, pops only the front, and
//     always leaves at least the just-pushed batch's last line. (An
//     earlier free-standing over-approximation could strand the ring
//     below a stored row end this replica itself wrote — a state the
//     real eviction cannot produce — and the first lineSpanExact
//     counterexample was reduced through that unreachable state; see
//     pushBatchAccepted.);
//   - the periodic and final flushes, including the once-per-tenure
//     stored-coverage reconciliation, the recovered-prefix gap-merge fold,
//     the true-line-span row arithmetic, the frozen-row UPSERT latch, and
//     the deferred-final tenure pin;
//   - the lease-composition interface (assume–guarantee against
//     leaderElection.qnt — see the LEASE INTERFACE section below);
//   - acquisition-time recovery: the prefix re-arm, the restamp from the
//     persisted assignment set, the stale-entry sweep, and the
//     load-failure / discarded outcomes;
//   - every entry reap path: the dispatch discard, the acquisition sweep,
//     the disconnect discard, the terminal-cleanup discard, and the four
//     flusher/epilogue reaps (tenure-orphan, refused-UPSERT, sealed-empty,
//     enqueue-failure).
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced):
//   - Cross-replica finalization races (two flushers writing one
//     `drv_logs` row). Model B's subject. The interim leader appears here
//     only through its PG-visible effects (`interimLeader*` actions).
//   - The flusher's await-granularity interleavings: `flush_final` is one
//     atomic action per outcome branch, so a lease transition can land
//     BETWEEN a request being enqueued and its attempt (caught by the
//     entry-time tenure pin) but not BETWEEN the attempt's guard SELECT
//     and its drain (the post-await re-checks collapse into the entry
//     pin). The check-await-act-on-stale bug class inside one attempt is
//     Phase 3's calibration subject (design §3.6).
//   - Line content, byte sizes, the numeric values of the ring caps, and
//     the u64-overflow ingestion arm (`line_number_overflow`) — the line
//     domain is 0..MAX_LINE-1 so overflow is unrepresentable; the
//     overflow rejection is a Kani obligation (design §3.5).
//   - The poison-TTL discard (`tick_process_expired_poisons`): a 24h
//     backstop, timescale-separated from every other event here. Excluding
//     a reap path makes `everyRetainedEntryIsJustified` strictly harder to
//     satisfy, so the exclusion is sound for the completeness claim.
//   - The read path (`GetDerivationLogs`), worker-side batching, and the
//     `build_derivations.exec_id` correlation row.
//
// LEASE INTERFACE (assume–guarantee against leaderElection.qnt): this
// model does NOT re-derive the election protocol. The lease actions'
// postconditions are exactly the properties the lease model proves:
//   - a holder change strictly increases the Lease's transition count
//     (apiserver-atomic with the holder change), so a re-acquire after a
//     foreign term records a strictly higher `acquiredTransitions`;
//   - the generation is monotone (`fetch_max`) and MAY OR MAY NOT move on
//     a holder change — once the recovery floor seed has saturated it past
//     `leaseTransitions + 1`, `on_acquire`'s fetch_max is a no-op
//     (leaderElection.qnt's deletion regime is where that divergence
//     arises; here it is the `recoveryRuns` seed-bump nondeterminism);
//   - a same-holder re-acquire (self-fence false alarm + successful renew)
//     moves neither the transition count nor the generation;
//   - a rebound moves the recorded transition count while `isLeader` stays
//     true and no lose edge ever fires locally.
// If leaderElection.qnt changes, this checklist is what gets re-derived.
//
// ENCODING DECISIONS:
//   - Lines are sets of integers (`Set[int]` over 0..MAX_LINE-1), not
//     content. Every line operation in the code is interval algebra over
//     worker-assigned line numbers — append above the high-water, reject
//     at-or-below it, evict a head prefix, truncate below a stored end,
//     count the holes — and none inspects content, so the set-of-numbers
//     abstraction is exact for everything the invariants observe
//     (design §3.2).
//   - The stored `drv_logs` row carries its recorded `(first_line,
//     line_count)` AND a ghost `trueEnd` (one past the highest true worker
//     line the row's content covers). The code's span arithmetic is
//     supposed to keep `first_line + line_count == trueEnd`
//     (obs.log.gap-span+2); `lineSpanExact` checks it. Encoding the row as
//     a bare interval would make that invariant true by construction and
//     verify nothing.
//   - `delivered` (the ghost per-execution conservation-law variable) is
//     the set of lines the ingestion gate ACCEPTED into the entry while it
//     was stamped with that execution — the per-replica reading of
//     obs.log.line-conservation. Lines a worker sent that the gate
//     rejected, and lines delivered only to another replica, are outside
//     the per-replica conservation law.
//   - There is no separate `suppressed` interval: `held` is an exact set,
//     so an evicted line is simply a member of `delivered` that is no
//     longer a member of `held`, and the conservation law quantifies over
//     `delivered` directly.
//   - One pending final-flush request slot per derivation
//     (`pendingFinal`), covering both "queued in the flush mpsc" and
//     "retained in the deferred-retry vector". A second terminal for the
//     same drv (a re-dispatched execution's own final) replaces the slot —
//     the same prefer-the-newer-tenure replacement `retain_deferred`
//     performs; the FIFO interleaving of two simultaneously queued finals
//     for the same drv is not explored.
//
// The invariants, their witnesses, and the per-regime non-vacuity
// procedures are documented at their definitions below. Measured state
// counts, depths, and wall-clocks live in the introducing commits'
// messages and the CI checks' output transcripts, never in this file.
module logBufferLifecycle {
  // --- 1. types ---------------------------------------------------------

  type Drv = str
  type Executor = str

  /// A `drv_logs` row, as the flush path writes and consults it.
  /// `lo`/`cnt` are the recorded `first_line`/`line_count` columns;
  /// `trueEnd` is the GHOST one-past-the-highest true worker line the
  /// row's content (blob + counted gaps) covers — what the recorded span
  /// is supposed to equal (`lineSpanExact`). `complete` is `is_complete`.
  type Row = { lo: int, cnt: int, trueEnd: int, complete: bool }
  type StoredRow = | NoRow | Stored(Row)

  /// Per-entry stored-coverage reconciliation state
  /// (`logs/mod.rs::PrefixState`). `Cached` carries the prior-tenure row's
  /// recorded span as fetched at reconcile time (`RecoveredPrefix`
  /// {first_line, line_count}) — the fold reads the CACHED snapshot on
  /// every subsequent flush of the tenure, not the live row.
  type Prefix = | PrefUnchecked | PrefChecked | PrefCached({ lo: int, cnt: int })

  /// A `LogBuffers` ring-buffer entry (`RingBuf`). Absence from the map is
  /// `NoEntry`. The model has no unstamped-entry state: the legacy
  /// `push()` `or_default()` path that creates one is test-only.
  /// `accountedBelow` is one past the highest line the entry has ever
  /// accounted for under its current execution stamp (`RingBuf::
  /// accounted_below`): the max of every accepted batch's end and the
  /// cached stored prefix's end. Never lowered — not by the
  /// stored-coverage truncation, not by eviction, not by the ring
  /// emptying; reset only by a cross-exec restamp.
  type EntrySlot =
    | NoEntry
    | Entry({
        exec: int,
        executor: Executor,
        held: Set[int],
        accountedBelow: int,
        finalPending: bool,
        prefix: Prefix,
      })

  /// A queued or retained final `FlushRequest`: the execution it is pinned
  /// to and the leadership-tenure stamps recorded at enqueue time
  /// (`FlushRequest::{exec_id, lease_generation, acquired_transitions}`).
  type Req = { exec: int, gen: int, transitions: int }
  type Pending = | NoFinal | Final(Req)

  /// The persisted assignment for a derivation (`assignments.exec_id` +
  /// the executor binding), as the next recovery would load it.
  /// `PgNone` = never dispatched, or its terminal status persisted.
  type PgAssign = | PgNone | PgLive({ exec: int, executor: Executor })

  /// The derivation's presence in this replica's in-memory DAG.
  /// `DagAbsent` = `hash_for_path` misses (never merged, reaped, or the
  /// DAG was wiped); `DagLive` = tracked non-terminal; `DagTerminal` =
  /// tracked terminal (awaiting `CleanupTerminalBuild`).
  type DagNode = | DagAbsent | DagLive | DagTerminal

  // --- 2. constants (const = overridable per regime) --------------------

  /// The derivation universe. Entries for distinct derivations share no
  /// state except the lease and the flusher loop, so each additional
  /// derivation roughly squares the state space; the regimes ship the
  /// smallest set that still reaches every per-entry interleaving class.
  const DRVS: Set[Drv]
  /// The executor universe. Two are required: the binding gate's
  /// foreign-executor rejection needs an executor that is NOT bound to
  /// the entry.
  const EXECUTORS: Set[Executor]
  /// Executions minted per derivation per trace (the dispatch /
  /// interim-re-dispatch ceiling). Two are required: the cross-exec
  /// restamp, the stale-request guard, and the re-dispatch reaps all need
  /// a second execution of the same derivation.
  const MAX_EXECS: int
  /// Worker line numbers range over 0..MAX_LINE-1. Three are required:
  /// an interior hole ({0,2}), a gap-merge fold (prefix [0,1) + ring {2}),
  /// and a head eviction ({0,1}→{1}) are the shapes the span arithmetic
  /// and the conservation law are about.
  const MAX_LINE: int
  /// Lease `leaseTransitions` ceiling (0 = no lease transitions: the
  /// replica holds the lease for the whole trace).
  const MAX_TRANSITIONS: int
  /// Generation ceiling (state-space bound; the generation is seeded from
  /// the transition count and the recovery floor, both bounded).
  const MAX_GEN: int
  /// Fault switches. Each gates one fault-injection branch; the regimes
  /// enable them in named combinations.
  const ENABLE_EVICTION: bool
  const ENABLE_LOAD_FAILURE: bool
  const ENABLE_DISCARDED_RECOVERY: bool
  const ENABLE_ENQUEUE_FAILURE: bool
  const ENABLE_PERSIST_FAILURE: bool
  const ENABLE_GUARD_FAILURE: bool
  /// Guard switch (NOT a fault switch): when true, the batch accept
  /// predicate rejects a batch starting below the entry's accounted
  /// high-water mark (`RingBuf::accounted_below` — the gate that keeps
  /// the gap-merge fold's "the ring starts at or after the cached
  /// prefix's end" precondition true at the trust boundary). When false,
  /// the predicate degrades to the pre-fix comparison against the ring's
  /// CURRENT tail only — the comparison that resets when the ring
  /// empties. Every shipped regime sets it true; the `Ungated` module
  /// sets it false and is wired as an expect-violation check proving the
  /// gate is load-bearing for `lineSpanExact` (deleting the guard
  /// re-falsifies the invariant).
  const ENABLE_ACCOUNTED_FLOOR: bool

  // Calibration switches (NOT fault switches). Each restores the
  // pre-fix behavior of one historical log-subsystem fix so the
  // override modules in docs/spec/models/calibration/ can demonstrate
  // that the fix is what keeps an invariant true — or record that no
  // model-A invariant observes its absence. Every shipped regime sets
  // every switch to the post-fix value (true unless noted); each
  // calibration module flips exactly one (the OneShotConsult module
  // flips two that revert to the same intermediate point in history).
  // The commit -> switch -> verdict table is log-invariant-map.md's
  // phase-3 findings section. ENABLE_ACCOUNTED_FLOOR above is
  // calibration entry #0 and follows the same pattern.

  /// true: every final-flush arm validates the request against the
  /// leadership tenure that enqueued it (`req_in_tenure`) and an
  /// out-of-tenure request is dropped. false: pre-3ce5d03e4 — every
  /// request is treated as in-tenure; the destructive arms run
  /// regardless of leadership and the drop arm is never enabled.
  const ENABLE_TENURE_PIN: bool
  /// true: the tenure pin is the OUTERMOST flush_final guard — the
  /// already-finalized refusal's residue drain, the guard-error arm's
  /// reap/retention, and the drop arm's orphan reap are all
  /// tenure-gated. false: pre-646022e37 — the refusal arms run for
  /// out-of-tenure requests too (exec-guarded only) and the drop arm
  /// reaps nothing.
  const ENABLE_TENURE_PIN_FIRST: bool
  /// true: a cross-exec restamp clears the retained lines and the
  /// accounted mark. false: pre-6c26e85f8 — set_exec overwrote only the
  /// stamp and the executor, carrying the prior execution's lines into
  /// the new execution's entry.
  const ENABLE_CROSS_EXEC_LINE_CLEAR: bool
  /// true: a cross-exec restamp clears the seal tombstone. false:
  /// pre-699ea1692 — the prior execution's seal survives over the new
  /// execution's entry and mutes its entire stream.
  const ENABLE_CROSS_EXEC_UNSEAL: bool
  /// true: a same-exec restamp clears the seal and the final-pending
  /// mark. false: pre-2f9a747d0 — both survive the restamp; the prior
  /// tenure's tenure-dropped final can never unseal the entry and the
  /// reconnected worker's post-flap batches are rejected.
  const ENABLE_SAME_EXEC_RESTAMP_CLEAR: bool
  /// true: the periodic flush's refused-UPSERT chokepoint reaps a
  /// sealed orphan whose execution another tenure finalized. false:
  /// pre-463090eb7/pre-f8ce10b8e — no recurring reaper exists for that
  /// orphan shape. Gates the reap action, its reapEnabledFor promise,
  /// and its half of the pre-recovery-window justification disjunct
  /// together — a justification promise about a reap that does not
  /// exist in the instantiated module would make the override
  /// trivially hold.
  const ENABLE_REAP_REFUSED_UPSERT: bool
  /// true: the periodic flush's empty-snapshot early return reaps a
  /// sealed empty entry. false: pre-81824cfbb — no reaper exists for a
  /// sealed entry whose ring the stored-coverage reconcile emptied
  /// after its pending final was consumed. Same three-site gating rule
  /// as ENABLE_REAP_REFUSED_UPSERT.
  const ENABLE_REAP_SEALED_EMPTY: bool
  /// true: the terminal epilogue reaps the zero-line entry whose final
  /// FlushRequest could not be enqueued. false: pre-6aab70b47 — the
  /// orphan waits for terminal cleanup / the periodic sealed-empty
  /// reap.
  const ENABLE_REAP_ENQUEUE_FAIL: bool
  /// 463090eb7's superseded mechanism: the one-shot read-only drv_logs
  /// consult at the tenure-drop arm (reap a sealed NON-empty orphan iff
  /// some tenure already finalized its execution, evaluated once at
  /// request-drop time, never re-run). FALSE everywhere except the
  /// OneShotConsult calibration module, which proves the intermediate
  /// fix was insufficient (the f8ce10b8e row's distinct evidence).
  const ENABLE_DROP_ARM_FINALIZED_CONSULT: bool
  /// true: the batch accept predicate requires the calling executor to
  /// be the one bound to the derivation's current execution. false:
  /// pre-496e6fb14 — the recv task called the legacy push() and never
  /// compared the calling stream's identity against assigned_executor.
  const ENABLE_BINDING_GATE: bool
  /// true: the disconnect-time log-buffer discard is gated on
  /// is_leader && dag_authoritative. false: pre-2c301438d — "not in the
  /// DAG" is read as "stale" even when the DAG was just wiped by a
  /// lease loss, destroying the retained in-flight entries the
  /// lease-flap retention preserves.
  const ENABLE_DISCONNECT_DAG_GATE: bool
  /// true: a successful recovery sweeps every retained unsealed entry
  /// whose derivation is not live in the rebuilt DAG. false:
  /// pre-e6c18add2 — acquisition-time reconciliation only restamps;
  /// an entry for a drv that went terminal under the interim leader is
  /// never discarded by any path.
  const ENABLE_ACQUISITION_SWEEP: bool
  /// true: the periodic UPSERT refuses to write a row another flush
  /// already finalized (the frozen-row latch as evaluated by a
  /// periodic write). false: pre-abfade4d5 — the DO UPDATE has no
  /// WHERE clause; a periodic .partial snapshot downgrades
  /// is_complete true->false and replaces the row's content. The
  /// refused-UPSERT reap is keyed on the refusal and degrades to
  /// never-reap with it.
  const ENABLE_PERIODIC_FROZEN_ROW: bool
  /// true: flush_final consults the drv_logs row before any
  /// destructive work and refuses to re-finalize an execution whose
  /// row is already complete. false: pre-44905298b — the consult does
  /// not exist and the UPSERT clause admits the true->true rewrite;
  /// a second final overwrites the finalized row with the retained
  /// residue's (smaller) span.
  const ENABLE_FINALIZE_REFUSAL: bool
  /// true: the gap-merge fold counts the prefix->ring gap itself in
  /// the recorded line_count. false: pre-d8944727e — the fold counts
  /// the single synthetic marker line instead, understating the span
  /// by gap - 1 whenever gap >= 2.
  const ENABLE_TRUE_GAP_SPAN: bool
  /// true: a flush payload's span contribution is its line-NUMBER span
  /// (last - first + 1). false: pre-effefb0a1 — it is the payload's
  /// physical line count, understating the span by the size of any
  /// interior hole.
  const ENABLE_RING_SPAN: bool
  /// true: the stored-coverage reconcile folds any non-finalized row
  /// the ring does not contiguously subsume. false: pre-e1fd179b9 —
  /// only a row wholly disjoint from (strictly below) the ring is
  /// folded; an overlapping row an interim leader extended past the
  /// ring's tail is overwritten by the ring-only snapshot.
  const ENABLE_OVERLAP_FOLD: bool

  /// `recoveredEpoch` sentinel: no epoch has completed recovery
  /// (rio-lease's `RECOVERY_NOT_COMPLETE`). -1 can never equal a real
  /// transition count (>= 0).
  pure val NO_EPOCH = -1

  // --- 3. pure helpers --------------------------------------------------

  /// One past the highest line in a non-empty set; 0 for the empty set.
  /// (`lines.back()` + 1 / `last_line + 1`.)
  pure def setEnd(s: Set[int]): int =
    if (s == Set()) 0 else s.fold(0, (a, b) => if (a > b) a else b) + 1
  /// The lowest line in a non-empty set; 0 for the empty set
  /// (`lines.front()` / `first_line`).
  pure def setLo(s: Set[int]): int =
    if (s == Set()) 0 else s.fold(MAX_LINE, (a, b) => if (a < b) a else b)
  /// `last_line` of a non-empty set (= setEnd - 1); 0 for the empty set.
  pure def setHi(s: Set[int]): int = if (s == Set()) 0 else setEnd(s) - 1
  pure def maxInt(a: int, b: int): int = if (a > b) a else b

  /// The `storedHist` ghost update for one (drv, exec) write of row `r`:
  /// snapshot the first finalizing write, raise the recorded-end
  /// high-water. Applied by every action that writes `stored`.
  pure def histAfterWrite(
    h: { frozen: StoredRow, everEnd: int },
    r: Row
  ): { frozen: StoredRow, everEnd: int } = {
    frozen: match h.frozen {
      | NoRow => if (r.complete) Stored(r) else NoRow
      | Stored(f) => Stored(f)
    },
    everEnd: maxInt(h.everEnd, r.lo + r.cnt),
  }

  // --- 4. state variables -----------------------------------------------

  /// `LogBuffers.buffers`: the per-derivation ring-buffer entries.
  var entries: Drv -> EntrySlot
  /// `LogBuffers.sealed`: the per-derivation terminal tombstones. A
  /// separate map because the seal can outlive the entry
  /// (`discard_if_empty` leaves a no-entry seal in place) and is checked
  /// before the entry lookup in `push_for`.
  var sealed: Drv -> bool
  /// The per-derivation pending final `FlushRequest` (queued or retained).
  var pendingFinal: Drv -> Pending
  /// Executions minted for each derivation so far (by this replica's
  /// dispatches and by interim leaders' re-dispatches). Execution N of a
  /// derivation is identified by the integer N (1-based).
  var dispatched: Drv -> int
  /// This replica's in-memory DAG view of each derivation.
  var dag: Drv -> DagNode
  /// The persisted assignment row for each derivation (what the next
  /// recovery's `load_dag_from_rows` restamp loop would see).
  var pg: Drv -> PgAssign
  /// GHOST, per (drv, exec): the lines the ingestion gate accepted into
  /// the entry while it was stamped with that execution. The
  /// conservation-law subject (obs.log.line-conservation).
  var delivered: Drv -> (int -> Set[int])
  /// The `drv_logs` row per (drv, exec) — the shared store, which in
  /// model A only this replica and the abstract interim leader write.
  var stored: Drv -> (int -> StoredRow)
  /// GHOST, per (drv, exec): the stored row's write history, which the
  /// frozen-row and coverage-monotonicity invariants compare the live row
  /// against. `frozen` is the row snapshotted by the FIRST write that set
  /// `is_complete = true` (`NoRow` until then); `everEnd` is the highest
  /// recorded span end (`first_line + line_count`) any write ever
  /// recorded. Raised at every `stored` write, never lowered, never read
  /// by an action guard. In the shipped regimes both fields are functions
  /// of the live row (every writer is guarded on `not(rowComplete)` and
  /// every write extends the recorded end), so the ghost adds no distinct
  /// states there; it diverges from the live row exactly when a
  /// calibration override removes one of those guards.
  var storedHist: Drv -> (int -> { frozen: StoredRow, everEnd: int })

  /// The lease-composition interface state (LeaderState's atomics), as
  /// one record so the actions that do not touch the lease frame it in
  /// one assignment:
  ///   - `isLeader`, `generation`, `acquiredTransitions`: the atomics
  ///     `on_acquire`/`on_rebound`/`on_lose` write.
  ///   - `recoveredEpoch`: `recovery_completed_for` — the acquire-epoch
  ///     stamp of the last completed recovery, or NO_EPOCH.
  ///     `recovery_complete()` is `recoveredEpoch == acquiredTransitions`.
  ///   - `loadSucceeded`: the actor's `dag_authoritative` bit — true only
  ///     after a tenure's recovery LOADED the DAG successfully (and the
  ///     TOCTOU gate did not discard it).
  var replica: {
    isLeader: bool,
    generation: int,
    acquiredTransitions: int,
    recoveredEpoch: int,
    loadSucceeded: bool,
  }
  /// The Lease object's global `leaseTransitions` count at the apiserver
  /// (holder changes so far, including foreign ones we have not observed).
  var leaseTransitions: int

  /// History flags (model-only bookkeeping, never read by an action
  /// guard; each is the discriminator for one expect-violation witness
  /// below). One record so the actions that fire none of them frame them
  /// in one assignment.
  ///   - `evicted`: an eviction dropped a non-empty head prefix of a held
  ///     ring.
  ///   - `rejectedNonMonotone`: the ingestion gate rejected a batch as
  ///     non-monotone.
  ///   - `crossExecCleared`: a cross-exec restamp cleared a non-empty
  ///     ring.
  ///   - `tenureDropReap`: `flush_final`'s out-of-tenure drop arm reaped a
  ///     sealed empty entry.
  ///   - `refusedUpsertReap`: the periodic refused-UPSERT reap fired
  ///     (frozen row ⇒ sealed orphan discarded).
  ///   - `sealedEmptyReap`: the periodic sealed-empty reap fired (empty
  ///     snapshot of a sealed entry).
  ///   - `enqueueFailReap`: the terminal epilogue's enqueue-failure reap
  ///     fired.
  ///   - `genPinnedAcrossHolderChange`: a holder change was re-acquired
  ///     with the generation unchanged (the saturated regime — the
  ///     `req_in_tenure` counterexample's premise).
  ///   - `alreadyFinalizedRefusal`: `flush_final`'s already-finalized
  ///     refusal arm fired (the row consult found another tenure's
  ///     completed row and refused to re-finalize it).
  var wit: {
    evicted: bool,
    rejectedNonMonotone: bool,
    crossExecCleared: bool,
    tenureDropReap: bool,
    refusedUpsertReap: bool,
    sealedEmptyReap: bool,
    enqueueFailReap: bool,
    genPinnedAcrossHolderChange: bool,
    alreadyFinalizedRefusal: bool,
  }

  // --- 5. derived state -------------------------------------------------

  def isLeader: bool = replica.isLeader
  def generation: int = replica.generation
  def acquiredTransitions: int = replica.acquiredTransitions

  /// `LeaderState::recovery_complete()`.
  def recoveryComplete: bool =
    replica.recoveredEpoch != NO_EPOCH and
      replica.recoveredEpoch == replica.acquiredTransitions

  /// `LogFlusher::may_flush()`: the gate on every self-driven flusher arm
  /// (periodic snapshot, GC sweep, shutdown sweep). Deliberately NOT
  /// conditioned on `loadSucceeded`: `set_recovery_complete` runs on the
  /// failed-load arm too, so a degraded empty-DAG tenure still snapshots
  /// its retained buffers.
  def mayFlush: bool = isLeader and recoveryComplete

  /// `dispatch_ready`'s leadership half: dispatch is gated on the lease
  /// AND on this tenure's recovery having completed.
  def dispatchReady: bool = isLeader and recoveryComplete

  /// `req_in_tenure(req)`: the request was enqueued by the leadership
  /// tenure this replica currently holds — leader AND both enqueue-time
  /// stamps still match. The generation alone cannot identify the tenure
  /// in the saturated regime (an A→B→A holder change is a generation
  /// no-op once the floor seed has raised it past `leaseTransitions + 1`);
  /// the acquire-epoch alone is belt-and-braces'd by the generation
  /// (a mid-tenure floor seed moves the generation without moving the
  /// epoch, though never after a request of that tenure is enqueued).
  /// With ENABLE_TENURE_PIN false this degrades to the pre-3ce5d03e4
  /// behavior: every request is treated as in-tenure, so the
  /// destructive final-flush arms are un-gated and `flushFinalDropStale`
  /// is never enabled.
  def reqInTenure(req: Req): bool = or {
    not(ENABLE_TENURE_PIN),
    and {
      isLeader,
      generation == req.gen,
      acquiredTransitions == req.transitions,
    },
  }

  /// The lines held by `d`'s entry iff it is stamped with `e`, else the
  /// empty set. The conservation law's "present in the in-memory ring"
  /// disjunct.
  def heldFor(d: Drv, e: int): Set[int] =
    match entries.get(d) {
      | NoEntry => Set()
      | Entry(en) => if (en.exec == e) en.held else Set()
    }

  // Entry accessors. Quint's effect system requires every branch of a
  // match used in action position to assign the same variables, so the
  // actions never match on the entry directly: they read it through these
  // accessors (which return safe defaults for `NoEntry` — the same -1 /
  // empty-set pattern leaderElection.qnt uses for its nullable snapshots)
  // and guard on `hasEntry` exactly where the code guards on the DashMap
  // lookup. The defaults are never observable: every action that reads an
  // accessor also conjoins the guard that makes the absent case disabled.
  def hasEntry(d: Drv): bool = entries.get(d) != NoEntry
  /// The entry's exec stamp; 1 (a valid ghost-map key) when absent.
  def entryExec(d: Drv): int =
    match entries.get(d) { | NoEntry => 1 | Entry(en) => en.exec }
  /// The entry's executor binding; "" (matches no executor) when absent.
  def entryExecutor(d: Drv): Executor =
    match entries.get(d) { | NoEntry => "" | Entry(en) => en.executor }
  def entryHeld(d: Drv): Set[int] =
    match entries.get(d) { | NoEntry => Set() | Entry(en) => en.held }
  def entryAccountedBelow(d: Drv): int =
    match entries.get(d) { | NoEntry => 0 | Entry(en) => en.accountedBelow }
  def entryFinalPending(d: Drv): bool =
    match entries.get(d) { | NoEntry => false | Entry(en) => en.finalPending }
  def entryPrefix(d: Drv): Prefix =
    match entries.get(d) { | NoEntry => PrefChecked | Entry(en) => en.prefix }
  /// Rebuild `d`'s entry with new held lines / accounted mark / pending
  /// mark / prefix state, keeping the stamp. Only meaningful under a
  /// `hasEntry` guard.
  def entryWith(d: Drv, held: Set[int], acct: int, fp: bool, pref: Prefix): EntrySlot =
    Entry({
      exec: entryExec(d),
      executor: entryExecutor(d),
      held: held,
      accountedBelow: acct,
      finalPending: fp,
      prefix: pref,
    })

  // --- 7. invariants ----------------------------------------------------
  //
  // The five model-A invariants of the build-log verification design
  // (§3.4) plus the three phase-3 calibration invariants (the stale-seal
  // muting dual, the frozen-row immutability half of finalize-immutable,
  // and the stored-coverage monotonicity half of
  // stored-coverage-preserved — each added because a historical fix's
  // calibration override produced a harmful state none of the original
  // five observed). Each carries the spec rule it verifies; the rule ↔
  // invariant correspondence is docs/spec/models/log-invariant-map.md and
  // the r[verify ...] markers live at the check wiring points in
  // nix/quint.nix. Defined after the actions they constrain so they can
  // reference the accessor defs; Quint definitions are order-independent.

  /// The runtime residual of TypeOK: the integer ranges the ceiling
  /// preconditions are supposed to enforce. A ceiling precondition that
  /// is dropped shows up here before it shows up as a non-terminating
  /// TLC run.
  val boundsOK = and {
    DRVS.forall(d => dispatched.get(d) >= 0 and dispatched.get(d) <= MAX_EXECS),
    DRVS.forall(d => entryHeld(d).forall(n => n >= 0 and n < MAX_LINE)),
    DRVS.forall(d => entryAccountedBelow(d) >= 0 and entryAccountedBelow(d) <= MAX_LINE),
    DRVS.forall(d => 1.to(MAX_EXECS).forall(e =>
      delivered.get(d).get(e).forall(n => n >= 0 and n < MAX_LINE))),
    DRVS.forall(d => hasEntry(d) implies
      (entryExec(d) >= 1 and entryExec(d) <= dispatched.get(d))),
    leaseTransitions >= 0,
    leaseTransitions <= MAX_TRANSITIONS,
    acquiredTransitions >= 0,
    acquiredTransitions <= leaseTransitions,
    generation >= 1,
    generation <= MAX_GEN,
  }

  /// `NoCrossExecContamination` (obs.log.exec-keyed+2 — the in-memory
  /// half; sched.log.batch-binding covers the ingress half): an entry
  /// stamped for execution E holds only lines delivered under E. A
  /// cross-exec restamp that carried the prior execution's lines into the
  /// new execution's entry would hand them to the flusher under the new
  /// execution's S3 key and `drv_logs` row; this is the invariant that
  /// falsifies if `set_exec`'s cross-exec arm stops clearing the ring.
  ///
  /// Non-vacuity: the `noCrossExecRestampClear` witness proves a
  /// cross-exec restamp of a NON-EMPTY ring is reachable in the flap
  /// regime — the case where the clearing is load-bearing.
  val noCrossExecContamination = DRVS.forall(d =>
    hasEntry(d) implies
      entryHeld(d).subseteq(delivered.get(d).get(entryExec(d))))

  /// `LineSpanExact` (obs.log.gap-span+2): a `drv_logs` row's recorded
  /// `first_line + line_count` is one past the highest true worker line
  /// the row's content covers — across gap-merged prefixes, interior
  /// holes, and plain contiguous payloads. Recording the physical line
  /// count instead (prefix physical + marker + ring physical) understates
  /// the end by the collapsed gap and breaks the read path's
  /// caught-up/re-serve decisions and the next failover's subsumption
  /// arithmetic. The recorded span is computed by the flush actions per
  /// the code's arithmetic; `trueEnd` is the ghost truth recorded
  /// alongside it.
  ///
  /// Non-vacuity: the `noRecoveredPrefix` witness proves the gap-merge
  /// fold (the arithmetic's hard case) is reachable in the flap regime;
  /// the `interiorHoleRun` deterministic run pins the holey-ring case.
  val lineSpanExact = DRVS.forall(d =>
    1.to(MAX_EXECS).forall(e =>
      match rowOf(d, e) {
        | NoRow => true
        | Stored(r) => r.lo + r.cnt == r.trueEnd
      }))

  /// `BindingGateExcludesForeignExecutors` (sched.log.batch-binding): the
  /// batch accept predicate admits a `(derivation, executor)` pair only
  /// when an entry exists for the derivation (a fabricated
  /// derivation_path must not allocate a buffer) and the calling
  /// executor is the one bound to the derivation's current execution.
  /// Quantified over the predicate's inputs rather than over a
  /// provenance ghost: the invariant falsifies if the executor or
  /// existence conjunct is dropped from `batchAccepted`, which is the
  /// model-side regression the rule guards against (the code-side TOCTOU
  /// — check the executor, then append after a restamp — is closed by
  /// the single shard write-lock and is a Kani/MBT obligation, not a
  /// protocol one).
  val bindingGateExcludesForeignExecutors = DRVS.forall(d =>
    EXECUTORS.forall(ex =>
      0.to(MAX_LINE - 1).forall(lo =>
        batchAccepted(d, ex, lo) implies and {
          hasEntry(d),
          entryExecutor(d) == ex,
        })))

  /// One SCHEDULER-INITIATED reap path's discard branch is enabled for
  /// `d`'s entry in this state — the entry has lost every justification
  /// and the next reap that observes it will discard it (the "one step
  /// from reaped" half of obs.log.entry-justified). The disjuncts are
  /// exactly the reap actions' discard-branch preconditions, restricted
  /// to the reaps the scheduler itself drives (the periodic tick, the
  /// acquisition sweep, the terminal cleanup, the pending final's own
  /// resolution). The disconnect-time discard is deliberately EXCLUDED:
  /// it is environment-initiated — it fires only if the owning worker's
  /// stream happens to close, which is not under the scheduler's control
  /// and may never happen for a long-lived connected worker — so "the
  /// disconnect discard is enabled" is not "the entry is one step from
  /// reaped". Conflating the two is exactly why the acquisition sweep
  /// (e6c18add2) was needed in the code even though the disconnect
  /// discard already covered the same (leader, DAG-absent, unsealed)
  /// state: an invariant that accepted the disconnect disjunct would
  /// declare the un-swept orphan justified forever.
  def reapEnabledFor(d: Drv): bool = or {
    // Terminal cleanup's post-delay discard (skips final-pending
    // entries).
    and { dag.get(d) == DagTerminal, not(entryFinalPending(d)) },
    // The acquisition sweep: unsealed and not live in the DAG the next
    // successful recovery rebuilds. Enabled only when a recovery is due.
    and {
      isLeader, not(recoveryComplete), not(sealed.get(d)),
      pg.get(d) == PgNone,
    },
    // The periodic sealed-empty reap. Gated on the same calibration
    // switch as the action: a justification promise about a reap that
    // does not exist in the instantiated module would make every
    // reap-disable override trivially hold.
    and { ENABLE_REAP_SEALED_EMPTY, mayFlush, sealed.get(d), entryHeld(d) == Set() },
    // The periodic refused-UPSERT reap (the row is already finalized, so
    // the durable record supersedes the retained lines). Same gating
    // rule.
    and {
      ENABLE_REAP_REFUSED_UPSERT,
      mayFlush, sealed.get(d), entryHeld(d) != Set(),
      rowComplete(d, entryExec(d)),
    },
    // The pending final's own resolution arms: the tenure-drop reap
    // (sealed + empty), the already-finalized drain, the empty drain,
    // and the upload all remove the entry when the request names its
    // stamped execution.
    match pendingFinal.get(d) {
      | NoFinal => false
      | Final(req) => req.exec == entryExec(d)
    },
  }

  /// `EveryRetainedEntryIsJustified` (obs.log.entry-justified): a
  /// retained entry is at all times either justified — its derivation is
  /// tracked non-terminal by this replica's DAG, a final flush for its
  /// stamped execution is pending and unresolved, the DAG is not
  /// authoritative for staleness decisions, its periodic reaper is
  /// suspended behind a recovery that is pending and will re-enable it,
  /// or it holds the only copy of lines no finalized durable record
  /// supersedes (the rule's intended fixed point: the periodic flush
  /// keeps them durable at `.partial` coverage) — or one reap path's
  /// discard branch is already enabled for it. The check is that the
  /// reap set is COMPLETE: there is no reachable entry state that is
  /// unjustified and that no reap path will ever observe (no fifth
  /// orphan shape).
  val everyRetainedEntryIsJustified = DRVS.forall(d =>
    hasEntry(d) implies or {
      // (1) The live carrier: tracked non-terminal by this replica's DAG.
      dag.get(d) == DagLive,
      // (2) A final flush for its stamped execution is pending and not
      // yet resolved.
      and {
        entryFinalPending(d),
        match pendingFinal.get(d) {
          | NoFinal => false
          | Final(req) => req.exec == entryExec(d)
        },
      },
      // (3) The DAG is not authoritative for staleness decisions: a
      // standby, a deposed leader, or a failed-load tenure. Entries are
      // held for the next authoritative reconciliation.
      not(isLeader and replica.loadSucceeded),
      // (3b) The periodic reaps' subject while their gate is closed and
      // the recovery that re-opens it is pending: a sealed entry in a
      // shape one of the two recovery-gated periodic reaps would discard
      // (empty -> the sealed-empty reap; non-empty with a finalized row
      // -> the refused-UPSERT reap), on a leader whose recovery
      // completion stamp has been cleared for the current acquire-epoch.
      // Keyed on the reap's own gate (`may_flush()`'s recovery_complete
      // half), NOT on `dag_authoritative`: a rebound — a foreign term
      // observed late on a still-leading round — is the one transition
      // that clears the completion stamp (suspending both periodic
      // reaps) while leaving is_leader, the DAG, and dag_authoritative
      // in place, so disjunct (3) does not fire and disjunct (5)'s reap
      // set goes empty for exactly these entries. The window is bounded
      // by one recovery cycle: `isLeader and not(recoveryComplete)` is
      // precisely the recovery actions' enabling condition (the code's
      // re-fired LeaderAcquired is already queued by on_rebound's
      // caller), the re-run re-stamps the completion for the new
      // acquire-epoch, may_flush() re-opens, and the next periodic tick
      // reaps the entry. Without the entry-shape conjuncts this would
      // accept ANY entry during the window, including one orphaned for
      // an unrelated reason; with them it accepts exactly the entries
      // whose only reaper is the suspended one.
      and {
        isLeader,
        not(recoveryComplete),
        sealed.get(d),
        // Each entry shape is keyed on its reap's calibration switch —
        // see the reapEnabledFor gating-rule comment.
        or {
          and { ENABLE_REAP_SEALED_EMPTY, entryHeld(d) == Set() },
          and { ENABLE_REAP_REFUSED_UPSERT, rowComplete(d, entryExec(d)) },
        },
      },
      // (4) The best-data-available fixed point: a non-empty ring whose
      // execution no tenure has finalized holds the only copy of those
      // lines.
      and { entryHeld(d) != Set(), not(rowComplete(d, entryExec(d))) },
      // (5) A reap that discards it is enabled now.
      reapEnabledFor(d),
    })

  /// `NoSilentLineLoss` (obs.log.line-conservation), the per-replica
  /// form: every line the ingestion gate accepted for an execution is, at
  /// all times, present in the in-memory ring, present in the execution's
  /// stored coverage, or counted in the stored row's claimed span as a
  /// marked or unmarked gap — and may only leave all three through a loss
  /// channel the stored record itself discloses: a recorded `first_line`
  /// above the line (head loss disclosed by the row's own start), or a
  /// record that never reaches `is_complete = true` (the incomplete
  /// indicator). The four disjuncts collapse arithmetically — every line
  /// below the recorded span's end is either in the span or below its
  /// start — so the bite is: a FINALIZED record's claimed span covers
  /// every line ever accepted, except those still held in a ring (which a
  /// finalizing drain empties).
  ///
  /// Non-vacuity: the `noCompletedRow` witness proves a finalized record
  /// is reachable in every regime — without it every disjunct but the
  /// last is dead and the invariant is vacuous.
  val noSilentLineLoss = DRVS.forall(d =>
    1.to(MAX_EXECS).forall(e =>
      delivered.get(d).get(e).forall(n => or {
        // In the in-memory ring of the entry stamped with this execution.
        heldFor(d, e).contains(n),
        // In the stored coverage, counted in the claimed span as a gap,
        // or below the recorded first_line (disclosed head loss).
        and { rowExists(d, e), n < rowEnd(d, e) },
        // The record never claims completeness: the incomplete indicator
        // is the disclosure (obs.log.incomplete-surfaced).
        not(rowComplete(d, e)),
      })))

  /// `NoStaleSealOnLiveCarrier` (obs.log.exec-keyed+2's restamp-unseal
  /// half; the muting dual of sched.log.batch-binding): a seal tombstone
  /// is the bridge between an execution's observed terminal and its
  /// final drain. An entry that is sealed while its derivation is
  /// tracked LIVE (non-terminal) by this replica's DAG is a stale seal
  /// muting a live execution: `push_for` checks the seal before the
  /// binding gate, so every batch of the entry's stamped execution is
  /// silently rejected — and the muting is invisible to the conservation
  /// law because a rejected line never enters `delivered`. Post-fix this
  /// is unreachable: every terminal seals and moves the DAG off
  /// `DagLive` in the same step, and every restamp (the only path that
  /// flips a sealed derivation back to `DagLive`) clears the seal. This
  /// is the invariant that falsifies if either restamp arm of `set_exec`
  /// stops clearing the seal — the 699ea1692 / 2f9a747d0 calibration
  /// entries in docs/spec/models/calibration/restamps.qnt.
  val noStaleSealOnLiveCarrier = DRVS.forall(d =>
    (sealed.get(d) and hasEntry(d)) implies dag.get(d) != DagLive)

  // NOT AN INVARIANT — the structural dual of noStaleSealOnLiveCarrier
  // ("a DagLive derivation always HAS an unsealed carrier", the
  // tenure-pins dossier's liveDerivationHasCarrier candidate) is
  // reachably FALSE on this model and is deliberately not defined here:
  // an in-tenure final flush legitimately drains the carrier of a
  // derivation whose DAG node is live only because the terminal's PG
  // persist failed and recovery rebuilt the node from the stale
  // assignment row. The harmful instance of the carrier-less DagLive
  // state (an out-of-tenure or wrong-exec request did the draining) is
  // state-identical to that benign one — the distinguishing fact is the
  // draining request's tenure stamps, which the drain consumes — so the
  // carrier-destruction half of the tenure-pins harm class is a
  // TRANSITION property model A cannot state. See
  // log-invariant-map.md's phase-3 findings and
  // ~/tmp/rio-formal-verification/phase3-falsification-liveDerivationHasCarrier.md.

  /// `FinalizedRowFrozen` (obs.log.finalize-immutable, the half
  /// observable under model A's writers — this replica plus the abstract
  /// interim): once any writer records a `drv_logs` row at
  /// `is_complete = true`, no later write changes any of its columns —
  /// content, span, or the complete bit itself. The code's enforcement
  /// is `upsert_drv_log`'s `WHERE NOT drv_logs.is_complete` chokepoint
  /// plus `flush_final`'s row consult; the model's enforcement is the
  /// `not(rowComplete)` guard on every `stored` writer. The ghost
  /// `storedHist.frozen` snapshots the first finalizing write; this
  /// invariant falsifies the moment any writer (the un-latched periodic
  /// UPSERT of abfade4d5^, the consult-free final of 44905298b^) touches
  /// the row again. The cross-replica blob/row split and the guard
  /// SELECT -> PUT window stay model B's (`AtMostOneFinalizingWrite`).
  val finalizedRowFrozen = DRVS.forall(d =>
    1.to(MAX_EXECS).forall(e =>
      match storedHist.get(d).get(e).frozen {
        | NoRow => true
        | Stored(f) => rowOf(d, e) == Stored(f)
      }))

  /// `StoredCoverageNeverRegresses` (obs.log.stored-coverage-preserved,
  /// the single-writer monotonicity half observable in model A): a
  /// `drv_logs` row's recorded span end (`first_line + line_count`)
  /// never decreases across writes. A flush that overwrites a row with a
  /// snapshot of a ring that does not reach the row's prior end destroys
  /// coverage of lines only the prior writer held (an interim leader's
  /// `.partial` of lines this replica never received) — the loss the
  /// stored-coverage reconcile (e1fd179b9) exists to prevent, invisible
  /// to the per-replica conservation law because those lines are not in
  /// this replica's `delivered` ghost. The cross-replica conservation
  /// form (the interim's delivered lines survive into the finalized
  /// record) is model B's; this is the monotonicity of the shared row
  /// under the writers model A has.
  val storedCoverageNeverRegresses = DRVS.forall(d =>
    1.to(MAX_EXECS).forall(e =>
      rowEnd(d, e) >= storedHist.get(d).get(e).everEnd))

  // --- expect-violation witnesses (non-vacuity probes) ------------------
  //
  // The vals below are NOT invariants and must never be added to a
  // regime check's invariant list. Each states "the interesting thing
  // never happens"; the quint-log-witness-* checks in nix/quint.nix
  // verify one each and PASS only when the checker reports a violation —
  // machine-checked evidence that the scenario a regime's headline
  // invariant constrains is actually reachable in that regime's explored
  // space. A witness that stops being violated means the model (or a
  // regime's constants) no longer reaches the behavior the code can
  // exhibit — a model bug, not a reason to delete the witness.

  /// Violated when a final flush completes a `drv_logs` row. The
  /// headline non-vacuity probe: `noSilentLineLoss` only bites for
  /// finalized records and `lineSpanExact` only bites for written rows,
  /// so a regime that never completes a row proves nothing.
  val noCompletedRow = not(DRVS.exists(d =>
    1.to(MAX_EXECS).exists(e => rowComplete(d, e))))

  /// Violated when the stored-coverage reconciliation caches a recovered
  /// prefix — the gap-merge fold (the span arithmetic's hard case) is
  /// actually exercised, not merely encoded.
  val noRecoveredPrefix = not(DRVS.exists(d =>
    match entryPrefix(d) { | PrefCached(p) => true | PrefUnchecked => false | PrefChecked => false }))

  /// Violated when a final flush request is pending while the leadership
  /// tenure that enqueued it has ended — the state the deferred-final
  /// tenure pin exists to catch (obs.log.deferred-final-retry+4's drop
  /// obligation is about exactly this request).
  val noFinalPendingAcrossTenures = not(DRVS.exists(d =>
    match pendingFinal.get(d) {
      | NoFinal => false
      | Final(req) => not(reqInTenure(req))
    }))

  /// Violated when an accepting push's eviction drops a non-empty head
  /// prefix of the appended ring (the push-coupled byte-cap eviction
  /// actually fires).
  val noEviction = not(wit.evicted)
  /// Violated when the ingestion gate rejects a batch numbered below what
  /// the entry has already accounted for.
  val noNonMonotoneRejection = not(wit.rejectedNonMonotone)
  /// Violated when a cross-exec restamp clears a non-empty ring (the
  /// clearing `noCrossExecContamination` depends on is load-bearing, not
  /// merely reachable on empty rings).
  val noCrossExecRestampClear = not(wit.crossExecCleared)
  /// Violated when `flush_final`'s out-of-tenure drop arm reaps a sealed
  /// empty entry (orphan shape 1 of 4).
  val noTenureDropReap = not(wit.tenureDropReap)
  /// Violated when the periodic refused-UPSERT reap fires (orphan shape
  /// 2 of 4).
  val noRefusedUpsertReap = not(wit.refusedUpsertReap)
  /// Violated when the periodic sealed-empty reap fires (orphan shape 3
  /// of 4).
  val noSealedEmptyReap = not(wit.sealedEmptyReap)
  /// Violated when the terminal epilogue's enqueue-failure reap fires
  /// (orphan shape 4 of 4).
  val noEnqueueFailReap = not(wit.enqueueFailReap)
  /// Violated when `flush_final`'s already-finalized refusal arm fires —
  /// the non-vacuity probe for the finalize-once guard partition
  /// (obs.log.finalize-immutable): without it nothing proves the
  /// post-44905298b consult arm is reachable and the
  /// `finalizedRowFrozen` verdict could be vacuous for the final path.
  /// Reachable only in the fault-persist regime (the only one in which
  /// a finalized row can coexist with a pending in-tenure final for the
  /// same execution).
  val noAlreadyFinalizedRefusal = not(wit.alreadyFinalizedRefusal)
  /// Violated when a holder change is re-acquired with the generation
  /// unchanged — the saturated regime in which the generation alone
  /// cannot identify the tenure and the acquire-epoch half of
  /// `req_in_tenure` is load-bearing.
  val noGenPinnedHolderChange = not(wit.genPinnedAcrossHolderChange)

  // --- 6. actions -------------------------------------------------------
  //
  // Each action's precondition is the corresponding code path's guard and
  // its effect is that path's state mutation; the function it mirrors is
  // named in its doc comment. Actions are GUARDED (disabled when their
  // preconditions fail), not thin always-enabled no-ops. Ceilings
  // (MAX_EXECS, MAX_LINE, MAX_TRANSITIONS, MAX_GEN) are action
  // preconditions, never saturating clamps.

  // --- 6a. dispatch -----------------------------------------------------

  /// `assign_to_worker` (actor/dispatch.rs): transition the drv to
  /// Assigned, `discard_log_buffer` (remove the entry AND the seal — a
  /// stale seal from a poison-clear would silently drop the new attempt's
  /// pushes), mint a fresh per-execution UUIDv7, `set_log_exec` (create a
  /// fresh stamped entry), and `record_assignment` (persist the
  /// assignment + exec_id for the next recovery's restamp loop).
  ///
  /// Covers the first dispatch, the transient-retry re-dispatch, the
  /// poison-clear resubmit, and the post-terminal-reset re-dispatch — all
  /// of them discard whatever entry the previous attempt left and start
  /// from a fresh stamped, unsealed, empty entry.
  ///
  /// Gated on `dispatch_ready` (leader + this epoch's recovery complete).
  /// The executor is the environment's choice.
  action dispatch(d: Drv, ex: Executor): bool = all {
    dispatchReady,
    dispatched.get(d) < MAX_EXECS,
    val nextExec = dispatched.get(d) + 1
    all {
      dispatched' = dispatched.put(d, nextExec),
      entries' = entries.put(d, Entry({
        exec: nextExec,
        executor: ex,
        held: Set(),
        accountedBelow: 0,
        finalPending: false,
        prefix: PrefUnchecked,
      })),
      sealed' = sealed.put(d, false),
      dag' = dag.put(d, DagLive),
      pg' = pg.put(d, PgLive({ exec: nextExec, executor: ex })),
    },
    pendingFinal' = pendingFinal,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  // --- 6b. batch ingestion ----------------------------------------------

  /// The accept predicate of `LogBuffers::push_for` for a non-empty batch
  /// `[lo, hi)` from `ex` for `d`'s entry, evaluated against the current
  /// state. The conjuncts, in the code's order:
  ///   1. not sealed (checked before the entry lookup);
  ///   2. an entry exists (`no_assignment` otherwise — a fabricated
  ///      derivation_path must not allocate a buffer);
  ///   3. the entry's `assigned_executor` matches the calling stream's
  ///      (`executor_mismatch` otherwise) — r[sched.log.batch-binding];
  ///   4. the batch's `first_line_number` is at or above the entry's
  ///      accounted high-water mark (`non_monotonic` /
  ///      `below_stored_prefix` otherwise) — the mark survives the ring
  ///      emptying and the stored-coverage truncation, so a re-delivered
  ///      line a cached stored prefix already covers is rejected instead
  ///      of re-entering the ring below the prefix end —
  ///      r[sched.executor.input-bounds+2] r[obs.log.gap-span+2].
  /// With ENABLE_ACCOUNTED_FLOOR false, conjunct 4 degrades to the
  /// pre-fix comparison against the ring's CURRENT tail (which resets
  /// when the ring empties) — the `Ungated` module's expect-violation
  /// check proves that degradation re-falsifies `lineSpanExact`.
  /// The `line_number_overflow` arm is unrepresentable in the bounded
  /// line domain (a Kani obligation, not a model one). The `unstamped`
  /// arm is unreachable (the model has no legacy `push()` entries).
  def batchAccepted(d: Drv, ex: Executor, lo: int): bool = and {
    not(sealed.get(d)),
    hasEntry(d),
    // Conjunct 3, the (executor, drv) binding gate. With
    // ENABLE_BINDING_GATE false the predicate degrades to the
    // pre-496e6fb14 recv task, which appended any unsealed batch via
    // the legacy push() without comparing the calling stream's
    // identity against the entry's binding.
    not(ENABLE_BINDING_GATE) or entryExecutor(d) == ex,
    if (ENABLE_ACCOUNTED_FLOOR)
      lo >= entryAccountedBelow(d)
    else
      entryHeld(d) == Set() or lo > setHi(entryHeld(d)),
  }

  /// `LogBuffers::push_for` + `push_into`, accept path: the batch's lines
  /// are appended to the ring, the accounted high-water mark is raised to
  /// the batch's end, and the lines are (ghost) attributed to the entry's
  /// current execution. The batch is the contiguous worker interval
  /// `[lo, hi)`; an upward gap between the ring's current end and `lo` is
  /// accepted and becomes an interior hole (obs.log.gap-span).
  /// The check and the write happen under one shard write-lock in the
  /// code, so accept-then-append is one atomic step here too.
  ///
  /// The line/byte-cap eviction is part of this step, not a free-standing
  /// action: the real eviction runs only inside `push_into`, after the
  /// append, pops only the front, and always leaves at least the
  /// just-pushed batch's last line (a line is capped at MAX_LINE_LEN <
  /// RING_BYTE_CAP, so the byte loop always stops with one line left).
  /// The earlier free-standing `evict` over-approximation could strand
  /// the ring's surviving tail strictly below a stored row end this
  /// replica itself wrote — a state the push-coupled real eviction cannot
  /// produce (the surviving back is the just-pushed line, the gate forces
  /// every push above the previous back, and a row this replica writes
  /// ends at its ring's back + 1) — and the original lineSpanExact
  /// counterexample exploited exactly that unreachable state. The cut
  /// point (the new front) is any held line at or below the just-pushed
  /// last line; cut == the current minimum means no eviction fired.
  /// Survivors stay a suffix, so the next flush's `first_line` reveals
  /// exactly which head range was evicted — the disclosed head-loss
  /// channel of obs.log.line-conservation.
  // Mirrors: LogBuffers::push_for (accept), LogBuffers::push_into
  //          (append + the `while ... pop_front` eviction loop).
  action pushBatchAccepted(d: Drv, ex: Executor, lo: int, hi: int): bool = {
    val lines = lo.to(hi - 1)
    val e = entryExec(d)
    val newHeld = entryHeld(d).union(lines)
    all {
      lo < hi,
      hi <= MAX_LINE,
      batchAccepted(d, ex, lo),
      // ENVIRONMENT ASSUMPTION (not a code guard — push_for accepts such
      // a batch): a worker's line numbers for an execution never reach a
      // finalized record's recorded end. The worker numbers lines
      // monotonically per execution and delivers every line before its
      // completion report on the same ordered stream; whichever tenure
      // observes that report finalizes with a recorded span that covers
      // the execution's true end (obs.log.gap-span+2), so a line at or
      // above a finalized end is one the build never produced. Without
      // this assumption the adversarial worker fabricates lines above a
      // finalized record's end and noSilentLineLoss is falsified by
      // content that never existed. A malicious worker that does
      // fabricate such lines loses them silently — the conservation law
      // is a promise about real build output, and the fabricated-input
      // surface is the fuzz target's and sched.executor.input-bounds'
      // subject, not this model's.
      not(rowComplete(d, e)) or hi <= rowEnd(d, e),
      // The eviction's cut point: the new front after the append. Must
      // keep the just-pushed batch's last line (`hi - 1`); equals the
      // post-append minimum when no eviction fires, which is the only
      // enabled choice when the eviction switch is off.
      nondet cut = newHeld.oneOf()
      all {
        cut <= hi - 1,
        ENABLE_EVICTION or cut == setLo(newHeld),
        entries' = entries.put(d, entryWith(
          d,
          newHeld.filter(n => n >= cut),
          maxInt(entryAccountedBelow(d), hi),
          entryFinalPending(d),
          entryPrefix(d))),
        wit' = if (cut > setLo(newHeld)) { ...wit, evicted: true } else wit,
      },
      delivered' = delivered.put(d,
        delivered.get(d).put(e, delivered.get(d).get(e).union(lines))),
      sealed' = sealed,
      pendingFinal' = pendingFinal,
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      stored' = stored,
      storedHist' = storedHist,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
    }
  }

  /// `LogBuffers::push_for`, line-numbering reject arm: a non-empty batch
  /// whose `first_line_number` is below what the entry has already
  /// accounted for is dropped — per-batch, no state change beyond the
  /// rejection counter (here: the witness flag). Covers both reasons the
  /// code partitions the arm into (`non_monotonic`: at or below the
  /// ring's current tail; `below_stored_prefix`: below the accounted mark
  /// while the ring holds nothing at or above the batch). The other
  /// reject arms (sealed, no entry, foreign executor) are pure no-ops and
  /// are not modeled as steps — a no-op self-loop adds no reachable state
  /// and the binding-gate invariant is about what `pushBatchAccepted`
  /// ADMITS, not about the rejections being observable.
  // Mirrors: LogBuffers::push_for (reason="non_monotonic" /
  //          reason="below_stored_prefix").
  action pushBatchRejectedNonMonotone(d: Drv, ex: Executor, lo: int): bool = all {
    not(sealed.get(d)),
    hasEntry(d),
    entryExecutor(d) == ex,
    if (ENABLE_ACCOUNTED_FLOOR)
      lo < entryAccountedBelow(d)
    else
      and { entryHeld(d) != Set(), lo <= setHi(entryHeld(d)) },
    lo >= 0,
    lo < MAX_LINE,
    wit' = { ...wit, rejectedNonMonotone: true },
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
  }

  // --- 6d. terminal + cleanup -------------------------------------------

  /// The execution a terminal for `d` is attributed to:
  /// `exec_id_for_terminal` resolves the DAG node's carrier or the
  /// buffer's stamp; the model uses the buffer's stamp when an entry
  /// exists (the carrier the flush request actually pins) and the latest
  /// minted execution otherwise.
  def terminalExec(d: Drv): int =
    if (hasEntry(d)) entryExec(d) else dispatched.get(d)

  /// `terminal_log_epilogue` (actor/event.rs) on a derivation reaching a
  /// terminal state with an execution to finalize, plus the terminal
  /// transition itself: seal the entry, enqueue the final `FlushRequest`
  /// stamped with the CURRENT lease generation and acquire-epoch
  /// (`trigger_log_flush`), mark the entry final-pending
  /// (`mark_final_pending`, exec-guarded), and persist the terminal
  /// status to PG. Success, permanent failure, and build-level
  /// cancellation all route through the same epilogue; the status string
  /// is not modeled (no invariant reads it).
  ///
  /// A second terminal for the same derivation (a re-dispatched
  /// execution's own terminal) replaces the pending request slot — the
  /// prefer-the-newer-tenure replacement `retain_deferred` performs.
  // Mirrors: terminal_log_epilogue + seal_log_buffer + trigger_log_flush
  //          + mark_final_pending; the terminal-status persist in the
  //          completion/cancel handlers.
  action terminalEnqueued(d: Drv): bool = all {
    dag.get(d) == DagLive,
    dispatched.get(d) > 0,
    val e = terminalExec(d)
    all {
      pendingFinal' = pendingFinal.put(d, Final({
        exec: e,
        gen: generation,
        transitions: acquiredTransitions,
      })),
      // mark_final_pending is exec-guarded: applied only when the entry
      // is stamped with the execution the request pins.
      entries' = if (hasEntry(d) and entryExec(d) == e)
        entries.put(d, entryWith(d, entryHeld(d), entryAccountedBelow(d), true, entryPrefix(d)))
      else entries,
    },
    sealed' = sealed.put(d, true),
    dag' = dag.put(d, DagTerminal),
    // The terminal-status persist. PG now records the derivation as
    // terminal, so the next recovery will not reload it as a live
    // assignment.
    pg' = pg.put(d, PgNone),
    dispatched' = dispatched,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `terminal_log_epilogue`'s enqueue-failure arm: the flush channel is
  /// full, the flusher task is dead, or no flusher is configured —
  /// `try_log_flush` returns false. No request is enqueued and no
  /// final-pending mark is set; a zero-line entry is reaped on the spot
  /// (`discard_if_empty` — the flusher's drain will never remove it, the
  /// periodic snapshot skips zero-line entries, and the drv is terminal
  /// so no re-dispatch discard is coming). A non-empty entry keeps the
  /// documented degraded mode (periodic `.partial` snapshots until
  /// `CleanupTerminalBuild`).
  /// r[obs.log.entry-justified] (prose reference; the tracey marker lives
  /// at the check wiring point in nix/quint.nix).
  // Mirrors: terminal_log_epilogue (enqueued == false) +
  //          LogBuffers::discard_if_empty.
  action terminalEnqueueFailed(d: Drv): bool = all {
    ENABLE_ENQUEUE_FAILURE,
    dag.get(d) == DagLive,
    dispatched.get(d) > 0,
    // Pre-6aab70b47 the epilogue could not tell the enqueue failed
    // (try_log_flush returned ()) and reaped nothing.
    val emptyReap = and { ENABLE_REAP_ENQUEUE_FAIL, hasEntry(d), entryHeld(d) == Set() }
    all {
      entries' = if (emptyReap) entries.put(d, NoEntry) else entries,
      // discard_if_empty clears the seal it just set only when it removed
      // an entry; otherwise the seal stays (it still guards late batches
      // and CleanupTerminalBuild bounds it).
      sealed' = sealed.put(d, not(emptyReap)),
      wit' = if (emptyReap) { ...wit, enqueueFailReap: true } else wit,
    },
    dag' = dag.put(d, DagTerminal),
    pg' = pg.put(d, PgNone),
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
  }

  /// The terminal-status persist failing (PG unreachable at terminal
  /// time): the in-memory DAG records the terminal and the epilogue still
  /// runs (seal + enqueue), but PG keeps the assignment as live — so the
  /// next tenure's recovery reloads the derivation as still
  /// Assigned/Running and restamps the SAME exec_id onto the retained
  /// entry. This is the correlated failure the deferred-final tenure pin
  /// exists for: the queued final and the un-persisted terminal are
  /// evidence of the same PG outage.
  // Mirrors: the completion handler's failed status UPDATE (the terminal
  //          proceeds in memory; PG is left stale).
  action terminalPersistFailed(d: Drv): bool = all {
    ENABLE_PERSIST_FAILURE,
    dag.get(d) == DagLive,
    dispatched.get(d) > 0,
    val e = terminalExec(d)
    all {
      pendingFinal' = pendingFinal.put(d, Final({
        exec: e,
        gen: generation,
        transitions: acquiredTransitions,
      })),
      entries' = if (hasEntry(d) and entryExec(d) == e)
        entries.put(d, entryWith(d, entryHeld(d), entryAccountedBelow(d), true, entryPrefix(d)))
      else entries,
    },
    sealed' = sealed.put(d, true),
    dag' = dag.put(d, DagTerminal),
    // The persist failed: PG still says Assigned/Running.
    pg' = pg,
    dispatched' = dispatched,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `handle_cleanup_terminal_build` (actor/build.rs), the log-buffer
  /// half: TERMINAL_CLEANUP_DELAY after a build completes, the reaped DAG
  /// nodes' entries are discarded — UNLESS the entry's final flush is
  /// still pending with the flusher (enqueued and unresolved, or deferred
  /// and retained), in which case the flusher's processing of that
  /// request is the entry's reaper. The DAG node itself is removed.
  // Mirrors: handle_cleanup_terminal_build (remove_build_interest_and_reap
  //          + the final_pending-gated discard loop).
  action cleanupTerminal(d: Drv): bool = all {
    dag.get(d) == DagTerminal,
    val discard = and { hasEntry(d), not(entryFinalPending(d)) }
    all {
      entries' = if (discard) entries.put(d, NoEntry) else entries,
      sealed' = if (discard) sealed.put(d, false) else sealed,
    },
    dag' = dag.put(d, DagAbsent),
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  // --- 6e. the flusher --------------------------------------------------

  /// Read accessors over the stored `drv_logs` row for one execution.
  def rowOf(d: Drv, e: int): StoredRow = stored.get(d).get(e)
  def rowExists(d: Drv, e: int): bool = rowOf(d, e) != NoRow
  def rowComplete(d: Drv, e: int): bool =
    match rowOf(d, e) { | NoRow => false | Stored(r) => r.complete }
  /// The row's recorded span end (`first_line + line_count`) — the
  /// stored-coverage end every subsumption and truncation decision is
  /// keyed on. 0 when no row exists.
  def rowEnd(d: Drv, e: int): int =
    match rowOf(d, e) { | NoRow => 0 | Stored(r) => r.lo + r.cnt }
  def rowLo(d: Drv, e: int): int =
    match rowOf(d, e) { | NoRow => 0 | Stored(r) => r.lo }
  def rowCnt(d: Drv, e: int): int =
    match rowOf(d, e) { | NoRow => 0 | Stored(r) => r.cnt }

  /// `lookup_stored_prefix`'s "the ring makes the stored row redundant"
  /// test: the ring contiguously covers the row's whole stated range —
  /// the same-tenure steady state, where this process wrote the row from
  /// this very ring. Anything short of that means the stored blob may
  /// hold lines this replica never had and must be folded in.
  def ringSubsumesRow(d: Drv, e: int): bool = and {
    val h = entryHeld(d)
    and {
      // ring_contiguous: a non-monotone or holey ring classifies as
      // non-contiguous — the conservative branch.
      h.size() == setHi(h) - setLo(h) + 1,
      setLo(h) <= rowLo(d, e),
      setHi(h) + 1 >= rowEnd(d, e),
    },
  }

  /// `reconcile_stored_prefix` + `lookup_stored_prefix` +
  /// `truncate_below`: the once-per-execution-per-tenure reconciliation
  /// of the ring against the stored row, run before the first non-empty
  /// flush of an execution on a tenure (`PrefixState::Unchecked`). When a
  /// non-finalized row exists that the ring does not contiguously
  /// subsume, the stored blob is fetched and cached as the execution's
  /// recovered prefix and the ring yields every line below the row's
  /// stated end (overlapping lines are superseded by the durable copy; a
  /// non-overlapping head below the stored range is the prior tenure's
  /// unflushed tail, inside the failover budget). Otherwise the entry is
  /// marked checked and nothing is fetched.
  ///
  /// A separate action (not folded into the flush actions) because the
  /// lookup and the GET are awaits: the environment may act between the
  /// reconcile settling the prefix state and the flush that consumes it.
  /// The flush actions require the prefix to be settled when the ring is
  /// non-empty, exactly as the code's inline `if Unchecked { reconcile }`
  /// blocks the flush until the reconcile resolves.
  // Mirrors: LogFlusher::reconcile_stored_prefix, lookup_stored_prefix,
  //          LogBuffers::truncate_below, set_recovered_prefix,
  //          mark_prefix_checked.
  action reconcileStoredPrefix(d: Drv): bool = all {
    hasEntry(d),
    entryPrefix(d) == PrefUnchecked,
    entryHeld(d) != Set(),
    val e = entryExec(d)
    val needsFold = and {
      rowExists(d, e),
      not(rowComplete(d, e)),
      // Post-e1fd179b9: fold anything the ring does not contiguously
      // subsume. Pre-fix: fold only a row wholly disjoint from
      // (strictly below) the ring — an overlapping row an interim
      // leader extended past the ring's tail was declared "not needed"
      // and the next flush overwrote it with the ring-only snapshot.
      if (ENABLE_OVERLAP_FOLD)
        not(ringSubsumesRow(d, e))
      else
        rowEnd(d, e) <= setLo(entryHeld(d)),
    }
    entries' = if (needsFold)
      // Found: cache the row's recorded span as the prefix, drop every
      // ring line below its stated end, and raise the accounted mark to
      // that end — every line below it is now durably accounted for by
      // the stored row, so a later batch numbered below it is rejected
      // instead of re-entering the ring under the cached prefix
      // (truncate_below and set_recovered_prefix both raise the mark in
      // the code; the model's reconcile is one atomic step so one raise
      // covers both).
      entries.put(d, entryWith(
        d,
        entryHeld(d).filter(n => n >= rowEnd(d, e)),
        maxInt(entryAccountedBelow(d), rowEnd(d, e)),
        entryFinalPending(d),
        PrefCached({ lo: rowLo(d, e), cnt: rowCnt(d, e) })))
    else
      // NotNeeded: no row, a finalized row (the already-finalized guard /
      // the frozen UPSERT own that), or the ring subsumes it.
      entries.put(d, entryWith(
        d, entryHeld(d), entryAccountedBelow(d), entryFinalPending(d), PrefChecked)),
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `upload_and_record`'s effective-row computation for a non-empty
  /// payload: the row's `(first_line, line_count)` stay in TRUE worker
  /// line-number space — the recovered prefix's span, the prefix→ring
  /// gap, and the ring's line-number span (NOT its physical count) are
  /// summed, so `first_line + line_count` is one past the highest true
  /// line the row covers even when the blob omits lines or replaces them
  /// with a marker (obs.log.gap-span+2). `trueEnd` is the ghost truth the
  /// `lineSpanExact` invariant compares the recorded span against.
  // Mirrors: upload_and_record (eff_first_line / eff_line_count /
  //          ring_span / the gap-merge fold).
  pure def mergedRow(pref: Prefix, ring: Set[int], complete: bool): Row = {
    val ringFirst = setLo(ring)
    val ringLast = setHi(ring)
    // The payload's span contribution. Post-effefb0a1 it is the
    // line-NUMBER span (a hole-carrying blob claims more lines than it
    // physically holds — the divergence the read path answers with a
    // full re-serve); pre-fix it was the physical line count, which
    // understates the recorded end by the size of any interior hole.
    val ringSpan = if (ENABLE_RING_SPAN) ringLast - ringFirst + 1 else ring.size()
    match pref {
      | PrefCached(p) =>
        // gap = first_line.saturating_sub(prefix_end)
        val prefixEnd = p.lo + p.cnt
        val gap = if (ringFirst > prefixEnd) ringFirst - prefixEnd else 0
        // The ghost truth: one past the highest true worker line the
        // row's CONTENT (prefix blob + ring lines) covers. The prefix's
        // own recorded span is exact (an inductive property of this very
        // fold), so its content reaches prefixEnd - 1; the ring reaches
        // ringLast. Under the accept gate's precondition (the ring starts
        // at or after the prefix end) the max is always ringLast + 1; the
        // max form keeps the ghost honest in the Ungated module, where a
        // ring stranded below the prefix end would otherwise UNDERSTATE
        // the truth and overstate the violation.
        {
          lo: p.lo,
          // Post-d8944727e the prefix->ring gap is counted in the
          // recorded line_count (the synthetic marker line stands in
          // for `gap` lost lines but is not itself counted); pre-fix
          // the marker was counted as 1, understating the span by
          // gap - 1 whenever gap >= 2.
          cnt: p.cnt + (if (ENABLE_TRUE_GAP_SPAN) gap else (if (gap > 0) 1 else 0)) + ringSpan,
          trueEnd: maxInt(prefixEnd, ringLast + 1),
          complete: complete,
        }
      | PrefUnchecked =>
        { lo: ringFirst, cnt: ringSpan, trueEnd: ringLast + 1, complete: complete }
      | PrefChecked =>
        { lo: ringFirst, cnt: ringSpan, trueEnd: ringLast + 1, complete: complete }
    }
  }

  /// `flush_periodic` + `upload_and_record(is_final=false)` for one
  /// derivation with a non-empty ring: snapshot (non-draining), upload
  /// the `.partial`, and UPSERT the row at `is_complete = false`. The
  /// frozen-row latch refuses the UPSERT when another tenure already
  /// finalized the execution; a refused UPSERT against a still-SEALED
  /// entry is the "finalized elsewhere" signal that reaps the orphan
  /// (`discard_if_sealed_for_exec(require_empty=false)` — the durable
  /// record supersedes the retained lines). An UNSEALED entry with a
  /// refused UPSERT is the live execution's carrier and is left alone.
  ///
  /// Gated on `may_flush` (leader + this epoch's recovery complete) and
  /// on the prefix state being settled (the code's inline reconcile).
  /// The per-derivation iteration of one tick is split into one action
  /// per derivation — a superset of the real interleavings (sound).
  /// r[obs.log.finalize-immutable] r[obs.log.entry-justified] (prose
  /// references; markers live at the check wiring points in
  /// nix/quint.nix).
  // Mirrors: LogFlusher::flush_periodic + upload_and_record (non-final,
  //          line_count > 0) + upsert_drv_log + the refused-UPSERT reap.
  action flushPeriodicUpload(d: Drv): bool = all {
    mayFlush,
    hasEntry(d),
    entryHeld(d) != Set(),
    entryPrefix(d) != PrefUnchecked,
    val e = entryExec(d)
    // Pre-abfade4d5 the DO UPDATE had no WHERE clause: nothing is
    // refused, the finalized row is downgraded and overwritten, and
    // (there being no refusal signal to key it on) nothing is reaped.
    val refused = and { ENABLE_PERIODIC_FROZEN_ROW, rowComplete(d, e) }
    val reap = and { ENABLE_REAP_REFUSED_UPSERT, refused, sealed.get(d) }
    val newRow = mergedRow(entryPrefix(d), entryHeld(d), false)
    all {
      stored' = if (refused) stored else
        stored.put(d, stored.get(d).put(e, Stored(newRow))),
      storedHist' = if (refused) storedHist else
        storedHist.put(d, storedHist.get(d).put(e,
          histAfterWrite(storedHist.get(d).get(e), newRow))),
      entries' = if (reap) entries.put(d, NoEntry) else entries,
      sealed' = if (reap) sealed.put(d, false) else sealed,
      wit' = if (reap) { ...wit, refusedUpsertReap: true } else wit,
    },
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
  }

  /// `upload_and_record`'s empty-snapshot early return: a periodic tick
  /// over a zero-line entry uploads nothing, but a SEALED empty entry
  /// stamped with this execution is reaped here — sealed means a terminal
  /// already fired and no restamp adopted the entry as the live carrier;
  /// empty means there is nothing left to persist (it never held a line,
  /// or this tick's stored-coverage reconcile truncated the whole ring
  /// away). Such an entry can never reach the PUT/UPSERT, no other reaper
  /// remains, and left in place it would sit in memory until process
  /// restart. An unsealed empty entry (a just-dispatched carrier waiting
  /// for its first batch) is never touched.
  /// r[obs.log.entry-justified] (prose reference).
  // Mirrors: upload_and_record (line_count == 0, !is_final) +
  //          discard_if_sealed_for_exec(require_empty=true).
  action flushPeriodicSealedEmptyReap(d: Drv): bool = all {
    // Pre-81824cfbb the empty-snapshot early return did nothing; the
    // whole action is the reap, so the switch disables it outright.
    ENABLE_REAP_SEALED_EMPTY,
    mayFlush,
    hasEntry(d),
    entryHeld(d) == Set(),
    sealed.get(d),
    entries' = entries.put(d, NoEntry),
    sealed' = sealed.put(d, false),
    wit' = { ...wit, sealedEmptyReap: true },
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
  }

  /// `flush_final`'s entry-time tenure pin, drop arm: a final flush
  /// request — first attempt or retained retry — may only finalize under
  /// the leadership tenure that enqueued it. A request that outlived its
  /// tenure is dropped with ZERO PG and S3 work; the only entry it reaps
  /// is one still SEALED for its exec (no restamp in the current tenure
  /// adopted it as the live carrier) and EMPTY (nothing a retry could
  /// upload). A sealed non-empty orphan is left for the periodic flush's
  /// refused-UPSERT or sealed-empty reaps; an unsealed entry may be the
  /// live execution's restamped carrier and is never touched.
  /// r[obs.log.deferred-final-retry+4] r[obs.log.entry-justified] (prose
  /// references).
  // Mirrors: flush_final (the !req_in_tenure entry pin) +
  //          discard_if_sealed_for_exec(require_empty=true).
  action flushFinalDropStale(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      pendingFinal.get(d) != NoFinal,
      not(reqInTenure(req)),
      val reap = and {
        // The drop arm's orphan reap arrived with 646022e37 (the
        // pin-outermost commit); pre-fix the drop arm reaped nothing.
        ENABLE_TENURE_PIN_FIRST,
        sealed.get(d),
        hasEntry(d),
        entryExec(d) == req.exec,
        or {
          entryHeld(d) == Set(),
          // 463090eb7's superseded one-shot finalized-elsewhere
          // consult, evaluated once at request-drop time and never
          // re-run. Removed by f8ce10b8e in favor of the recurring
          // refused-UPSERT reap; enabled only in the OneShotConsult
          // calibration module.
          and { ENABLE_DROP_ARM_FINALIZED_CONSULT, rowComplete(d, req.exec) },
        },
      }
      all {
        entries' = if (reap) entries.put(d, NoEntry) else entries,
        sealed' = if (reap) sealed.put(d, false) else sealed,
        wit' = if (reap) { ...wit, tenureDropReap: true } else wit,
      },
      pendingFinal' = pendingFinal.put(d, NoFinal),
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      stored' = stored,
      storedHist' = storedHist,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
    }
  }

  /// `flush_final`'s already-finalized refusal: the guard SELECT finds
  /// the execution's row already `is_complete = true` — another tenure
  /// finalized it. The retained residue is drained iff still stamped with
  /// this execution (and unsealed); nothing is uploaded and the durable
  /// record is left untouched. The retained lines it lacks fall within
  /// the accepted failover-loss bound and the loss is disclosed by the
  /// finalized record's own claimed span.
  /// r[obs.log.finalize-immutable] (prose reference).
  // Mirrors: flush_final (the Ok(Some(stored)) if stored.is_complete arm)
  //          + drain_if_exec + unseal.
  action flushFinalRefuseFinalized(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      pendingFinal.get(d) != NoFinal,
      // Pre-646022e37 the already-finalized guard SELECT ran BEFORE the
      // tenure pin, so this arm (and its residue drain) fired for
      // out-of-tenure requests too.
      not(ENABLE_TENURE_PIN_FIRST) or reqInTenure(req),
      // Pre-44905298b the row consult did not exist: this arm is the
      // consult, so disabling the refusal disables the whole arm.
      ENABLE_FINALIZE_REFUSAL,
      rowComplete(d, req.exec),
      val drained = and { hasEntry(d), entryExec(d) == req.exec }
      all {
        entries' = if (drained) entries.put(d, NoEntry) else entries,
        sealed' = if (drained) sealed.put(d, false) else sealed,
      },
      pendingFinal' = pendingFinal.put(d, NoFinal),
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      stored' = stored,
      storedHist' = storedHist,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
      wit' = { ...wit, alreadyFinalizedRefusal: true },
    }
  }

  /// `flush_final`'s finalize-guard-error arm: the row consult itself
  /// failed (PG unreachable), so the flusher cannot tell "first
  /// finalization" from "another leader already finalized this exec" and
  /// fails closed — nothing is uploaded. A zero-line entry stamped with
  /// the request's exec is reaped (nothing any retry could upload) and
  /// the request is consumed; a non-empty stamped entry re-asserts the
  /// pending mark and the request is RETAINED for retry on the next
  /// flusher tick (the request stays in the pending slot); no stamped
  /// entry means nothing a retry could drain — the request is consumed.
  /// r[obs.log.deferred-final-retry+4] (prose reference).
  // Mirrors: flush_final (the Err(e) guard arm) +
  //          discard_if_empty_for_exec + mark_final_pending +
  //          retain_deferred.
  action flushFinalGuardError(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      ENABLE_GUARD_FAILURE,
      pendingFinal.get(d) != NoFinal,
      // Pre-646022e37 the guard SELECT ran BEFORE the tenure pin, so
      // this arm's reap and retention fired for out-of-tenure requests
      // too (exec-guarded only).
      not(ENABLE_TENURE_PIN_FIRST) or reqInTenure(req),
      not(rowComplete(d, req.exec)),
      val stamped = and { hasEntry(d), entryExec(d) == req.exec }
      val emptyReap = and { stamped, entryHeld(d) == Set() }
      val retained = and { stamped, entryHeld(d) != Set() }
      all {
        entries' = if (emptyReap) entries.put(d, NoEntry)
          else if (retained)
            entries.put(d, entryWith(d, entryHeld(d), entryAccountedBelow(d), true, entryPrefix(d)))
          else entries,
        sealed' = if (emptyReap) sealed.put(d, false) else sealed,
        // Retained for retry: the request stays in the slot. Consumed
        // otherwise.
        pendingFinal' = if (retained) pendingFinal else pendingFinal.put(d, NoFinal),
      },
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      stored' = stored,
      storedHist' = storedHist,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
      wit' = wit,
    }
  }

  /// `flush_final`'s drain-and-upload arm for a non-empty ring: the
  /// in-tenure, not-yet-finalized request drains the entry stamped with
  /// its execution (`drain_if_exec` — the exec comparison and the removal
  /// are one atomic op, so a re-dispatch racing the queue cannot have its
  /// fresh entry drained by a stale request), unseals, uploads the final
  /// blob, and UPSERTs the row at `is_complete = true`. The row's span is
  /// the true-line-number merge of the cached prefix, the prefix→ring
  /// gap, and the ring's span (obs.log.gap-span+2).
  /// r[obs.log.exec-keyed+2] (prose reference).
  // Mirrors: flush_final (the drain_if_exec Some arm) + upload_and_record
  //          (is_final=true, line_count > 0) + upsert_drv_log.
  action flushFinalUpload(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      pendingFinal.get(d) != NoFinal,
      reqInTenure(req),
      // Pre-44905298b there was no row consult: a second final for an
      // already-finalized exec drained the residue and overwrote the
      // finalized row with the residue's (smaller) span.
      not(ENABLE_FINALIZE_REFUSAL and rowComplete(d, req.exec)),
      hasEntry(d),
      entryExec(d) == req.exec,
      entryHeld(d) != Set(),
      // The code's inline reconcile must have settled the prefix state
      // before the pre-drain read; the reconcile is a separate action
      // here, so the upload waits for it.
      entryPrefix(d) != PrefUnchecked,
      val newRow = mergedRow(entryPrefix(d), entryHeld(d), true)
      all {
        stored' = stored.put(d, stored.get(d).put(req.exec, Stored(newRow))),
        storedHist' = storedHist.put(d, storedHist.get(d).put(req.exec,
          histAfterWrite(storedHist.get(d).get(req.exec), newRow))),
      },
      entries' = entries.put(d, NoEntry),
      sealed' = sealed.put(d, false),
      pendingFinal' = pendingFinal.put(d, NoFinal),
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
      wit' = wit,
    }
  }

  /// `flush_final`'s empty-drain arm: the entry stamped with the
  /// request's execution drains zero lines (a failover restamp whose
  /// worker never re-streamed, or a silent build). Nothing is uploaded;
  /// `finalize_empty_drain` stamps `status`/`finished_at` on any prior
  /// `.partial` row WITHOUT setting `is_complete` — the stored content is
  /// truncated at the last periodic snapshot and the incomplete indicator
  /// must survive (obs.log.incomplete-surfaced). The entry is removed by
  /// the drain and unsealed.
  // Mirrors: flush_final (drain_if_exec Some, line_count == 0) +
  //          finalize_empty_drain.
  action flushFinalEmptyDrain(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      pendingFinal.get(d) != NoFinal,
      reqInTenure(req),
      // Same partition switch as flushFinalUpload's: pre-44905298b the
      // complete-row empty drain routed here instead of the refusal arm
      // (the modeled effects of the two arms coincide for an empty
      // entry — drain, unseal, consume the request, leave stored alone).
      not(ENABLE_FINALIZE_REFUSAL and rowComplete(d, req.exec)),
      hasEntry(d),
      entryExec(d) == req.exec,
      entryHeld(d) == Set(),
      // status/finished_at are not modeled; the row's content columns and
      // is_complete are untouched.
      entries' = entries.put(d, NoEntry),
      sealed' = sealed.put(d, false),
      pendingFinal' = pendingFinal.put(d, NoFinal),
      stored' = stored,
      storedHist' = storedHist,
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
      wit' = wit,
    }
  }

  /// `flush_final`'s stale-request / no-entry arm: the entry is gone or
  /// stamped with a different (newer) execution — a re-dispatch landed
  /// between queuing and processing. The request is dropped without
  /// draining anything (the live execution's entry and seal are left
  /// alone); when no entry exists at all the leftover seal is defensively
  /// cleared.
  // Mirrors: flush_final (the drain_if_exec None arm).
  action flushFinalStaleOrMissing(d: Drv): bool = {
    val req = match pendingFinal.get(d) { | NoFinal => { exec: 0, gen: 0, transitions: 0 } | Final(r) => r }
    all {
      pendingFinal.get(d) != NoFinal,
      reqInTenure(req),
      // Same partition switch as flushFinalUpload's (the complete-row
      // no-entry case routes here when the refusal arm does not exist).
      not(ENABLE_FINALIZE_REFUSAL and rowComplete(d, req.exec)),
      not(and { hasEntry(d), entryExec(d) == req.exec }),
      // No entry at all => defensive unseal; a live restamped entry's
      // seal belongs to the live execution and is left alone.
      sealed' = if (hasEntry(d)) sealed else sealed.put(d, false),
      pendingFinal' = pendingFinal.put(d, NoFinal),
      entries' = entries,
      stored' = stored,
      storedHist' = storedHist,
      dispatched' = dispatched,
      dag' = dag,
      pg' = pg,
      delivered' = delivered,
      replica' = replica,
      leaseTransitions' = leaseTransitions,
      wit' = wit,
    }
  }

  // --- 6f. lease transitions (the §3.3 abstract interface) --------------

  /// A foreign holder change at the apiserver: another replica's steal
  /// PUT bumps the Lease's `leaseTransitions` count. Our local state does
  /// not change — we have not observed it yet. Whether we later observe
  /// it through a lose/acquire edge pair or only late as a rebound is the
  /// environment's choice (the following actions).
  action foreignSteal: bool = all {
    leaseTransitions < MAX_TRANSITIONS,
    leaseTransitions' = leaseTransitions + 1,
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    wit' = wit,
  }

  /// `LeaderState::on_lose` + the actor's `handle_leader_lost`: the lease
  /// loop observes the loss (a 409, an observed foreign holder, or a
  /// self-fence), clears `is_leader` and the recovery-completion stamp,
  /// and the actor invalidates the completion and wipes the persisted
  /// in-memory state (`clear_persisted_state`: the DAG is emptied and
  /// `dag_authoritative` cleared). The LogBuffers entries are RETAINED —
  /// a still-streaming worker's in-flight execution keeps its lines
  /// across the flap; they are reconciled at the next acquisition.
  /// The lose and the actor's processing of the queued LeaderLost are
  /// collapsed into one step; the on_lose-to-dequeue window's only
  /// observable effect (a completion processed with `is_leader` already
  /// false) is equivalent to the completion landing just before the lose.
  // Mirrors: LeaderState::on_lose, DagActor::handle_leader_lost,
  //          clear_persisted_state.
  action loseLease: bool = all {
    isLeader,
    replica' = {
      ...replica,
      isLeader: false,
      recoveredEpoch: NO_EPOCH,
      loadSucceeded: false,
    },
    dag' = DRVS.mapBy(d => DagAbsent),
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `LeaderState::on_acquire` on a re-acquire that follows at least one
  /// foreign holder change: our steal PUT bumps the transition count once
  /// more, the new count is recorded unconditionally in
  /// `acquired_transitions`, and the generation is `fetch_max`'d to
  /// `transitions + 1` — a NO-OP when the recovery floor seed has already
  /// raised the generation past it (the saturated post-deletion regime).
  /// That no-op is the lease model's "the generation may or may not
  /// increase on a holder change" guarantee and the premise of the
  /// `req_in_tenure` acquire-epoch fix; the witness flag records when it
  /// happens. Recovery has not run yet for the new epoch — the
  /// fire-and-forgotten LeaderAcquired is the `recoveryRuns*` action.
  // Mirrors: LeaderState::on_acquire (the steal path).
  action reacquireAfterHolderChange: bool = all {
    not(isLeader),
    leaseTransitions > acquiredTransitions,
    leaseTransitions < MAX_TRANSITIONS,
    maxInt(generation, leaseTransitions + 2) <= MAX_GEN,
    val newCount = leaseTransitions + 1
    val newGen = maxInt(generation, newCount + 1)
    all {
      leaseTransitions' = newCount,
      replica' = {
        ...replica,
        isLeader: true,
        acquiredTransitions: newCount,
        generation: newGen,
      },
      wit' = if (newGen == generation)
        { ...wit, genPinnedAcrossHolderChange: true }
      else wit,
    },
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
  }

  /// `LeaderState::on_acquire` on a same-epoch re-acquire: a self-fence
  /// false alarm followed by a successful renew of a lease nobody stole
  /// in between. The transition count did not move, so the recorded
  /// acquire-epoch and the generation are both unchanged — the in-flight
  /// work (including a queued final flush) is still this tenure's to
  /// resolve, which is exactly why `req_in_tenure` deliberately keeps
  /// such a request in tenure. Recovery still re-runs (the lose cleared
  /// the completion stamp and wiped the DAG).
  // Mirrors: LeaderState::on_acquire (the renew-edge re-acquire path).
  action reacquireSameEpoch: bool = all {
    not(isLeader),
    leaseTransitions == acquiredTransitions,
    replica' = { ...replica, isLeader: true },
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `LeaderState::on_rebound`: a holder change observed late, on a
  /// still-leading round — a foreign term began and ended entirely inside
  /// our observation gap, so neither an acquire nor a lose edge ever
  /// fired locally. The completion stamp is cleared FIRST, then the new
  /// transition count is recorded, then the generation is fetch_max'd.
  /// `is_leader` is NOT touched and — the encoded surprise — neither is
  /// the actor's `dag_authoritative` bit nor the DAG: no LeaderLost ever
  /// fires, and the re-fired LeaderAcquired only clears them when the
  /// actor dequeues it (the `recoveryRuns*` action). Until then the
  /// disconnect-time discard's `is_leader && dag_authoritative` gate is
  /// still open over the pre-rebound DAG.
  // Mirrors: LeaderState::on_rebound.
  action reboundLease: bool = all {
    isLeader,
    leaseTransitions > acquiredTransitions,
    maxInt(generation, leaseTransitions + 1) <= MAX_GEN,
    replica' = {
      ...replica,
      acquiredTransitions: leaseTransitions,
      generation: maxInt(generation, leaseTransitions + 1),
      recoveredEpoch: NO_EPOCH,
    },
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  // --- 6g. acquisition-time recovery ------------------------------------

  /// `LogBuffers::set_exec`'s restamp semantics, as recovery applies them
  /// to one retained entry against the persisted assignment `(e, ex)`:
  ///   - no entry: create a fresh empty stamped entry (the carrier for
  ///     the periodic flush before the worker's first batch);
  ///   - same exec: keep the lines and the accounted high-water mark (a
  ///     single lease flap with no interim re-dispatch — the
  ///     still-streaming worker's execution must keep accumulating, and
  ///     its numbering does not restart), but clear the seal, the
  ///     pending-final mark, and the per-tenure prefix bookkeeping (the
  ///     prior tenure's retained final can no longer resolve the entry,
  ///     and an interim leader may have extended the stored row past what
  ///     this ring holds);
  ///   - cross exec: the assignment was re-issued under a different
  ///     exec_id while this entry sat retained — clear the lines, the
  ///     accounted mark (the new execution restarts its numbering at 0),
  ///     the seal, the pending-final mark, and the prefix state; carrying
  ///     any of them across would hand one execution's output to another
  ///     execution's S3 key and drv_logs row (obs.log.exec-keyed+2) or
  ///     mute the new execution's entire stream.
  pure def restampEntry(slot: EntrySlot, e: int, ex: Executor): EntrySlot =
    match slot {
      | NoEntry => Entry({
          exec: e, executor: ex, held: Set(), accountedBelow: 0,
          finalPending: false, prefix: PrefUnchecked,
        })
      | Entry(en) =>
        if (en.exec == e)
          Entry({
            exec: e, executor: ex, held: en.held, accountedBelow: en.accountedBelow,
            // Pre-2f9a747d0 the same-exec arm kept the mark; the prior
            // tenure's tenure-dropped final then never resolves it and
            // the mark exempts the entry from terminal cleanup forever.
            finalPending: if (ENABLE_SAME_EXEC_RESTAMP_CLEAR) false else en.finalPending,
            prefix: PrefUnchecked,
          })
        else if (ENABLE_CROSS_EXEC_LINE_CLEAR)
          Entry({
            exec: e, executor: ex, held: Set(), accountedBelow: 0,
            finalPending: false, prefix: PrefUnchecked,
          })
        else
          // Pre-6c26e85f8: set_exec overwrote only exec_id and
          // assigned_executor; the prior execution's lines and accounted
          // mark ride into the new execution's entry and the flusher
          // uploads them under the new execution's S3 key and row.
          Entry({
            exec: e, executor: ex, held: en.held, accountedBelow: en.accountedBelow,
            finalPending: false, prefix: PrefUnchecked,
          })
    }

  /// Whether restamping `slot` against persisted exec `e` changes the
  /// execution stamp (`restampDropsLines` minus the non-empty
  /// conjunct) — the discriminator for the per-arm seal-clear
  /// calibration switches: the cross-exec and same-exec restamp arms
  /// acquired their seal-clearing behavior in separate fixes
  /// (699ea1692 and 2f9a747d0) and are reverted separately.
  pure def restampIsCrossExec(slot: EntrySlot, e: int): bool =
    match slot {
      | NoEntry => false
      | Entry(en) => en.exec != e
    }

  /// Whether restamping `slot` against persisted exec `e` is a cross-exec
  /// restamp that drops a non-empty ring (the witness discriminator).
  pure def restampDropsLines(slot: EntrySlot, e: int): bool =
    match slot {
      | NoEntry => false
      | Entry(en) => en.exec != e and en.held != Set()
    }

  /// The post-recovery state of one derivation's entry under a successful
  /// load: restamped from the persisted assignment when one exists;
  /// otherwise re-armed (prefix cleared to Unchecked) and then swept by
  /// `discard_unsealed_not_in` iff unsealed (sealed entries are the
  /// flusher's to drain — their final flush may still be queued).
  pure def recoveredEntry(slot: EntrySlot, p: PgAssign, isSealed: bool): EntrySlot =
    match p {
      | PgLive(a) => restampEntry(slot, a.exec, a.executor)
      | PgNone =>
        // Pre-e6c18add2 there was no sweep at all: acquisition-time
        // reconciliation only restamped PG-live entries, so an entry
        // whose drv went terminal under the interim was never
        // discarded by any path (only the prefix re-arm applies).
        if (isSealed or not(ENABLE_ACQUISITION_SWEEP))
          // Spared by the sweep; the prefix re-arm still applies.
          match slot {
            | NoEntry => NoEntry
            | Entry(en) => Entry({ ...en, prefix: PrefUnchecked })
          }
        else NoEntry
    }

  /// `handle_leader_acquired` + `recover_from_pg`, the successful-load,
  /// gate-passing outcome: clear the persisted state, re-arm the
  /// stored-coverage reconciliation on every retained entry
  /// (`rearm_prefix_reconciliation`), reload the DAG from PG, restamp the
  /// entry of every persisted live assignment (`set_log_exec`), sweep
  /// every UNSEALED retained entry whose derivation is not live in the
  /// rebuilt DAG (`sweep_stale_log_buffers` — it went terminal or
  /// vanished under an interim leader and would otherwise shadow the
  /// stored log and be re-uploaded every periodic tick), seed the
  /// generation from the durable floor (which MAY raise it without
  /// moving the acquire-epoch — the mid-tenure floor-seed event shape
  /// that saturates the generation), mark the DAG authoritative, and
  /// record the epoch-keyed recovery completion.
  /// r[sched.recovery.log-buffer-sweep+2] r[obs.log.exec-keyed+2]
  /// r[obs.log.stored-coverage-preserved] (prose references).
  // Mirrors: handle_leader_acquired (the Ok + gate-pass arm),
  //          recover_from_pg, rearm_prefix_reconciliation, set_exec,
  //          sweep_stale_log_buffers / discard_unsealed_not_in,
  //          seed_generation_from, set_recovery_complete.
  action recoveryOk: bool = all {
    isLeader,
    not(recoveryComplete),
    // The floor seed: 0 = the durable floor vouches for the entry
    // generation (the ordinary failover — in a deletion-free history the
    // floor never exceeds the lease-derived generation, so this is the
    // only real outcome there); 1..2 = the floor exceeded it and the
    // claim target bumped past it. A positive bump is only reachable in
    // production after a `kubectl delete lease` reset the transition
    // count while the PG floor persisted; the model does not re-derive
    // that mechanism (it is leaderElection.qnt's deletion regime) — it
    // takes the resulting "the generation may move without the
    // acquire-epoch moving, and may then be a no-op on the next holder
    // change" guarantee as the §3.3 interface's nondeterminism. The
    // ceiling is a precondition, never a clamp.
    nondet seedBump = Set(0, 1, 2).oneOf()
    all {
      generation + seedBump <= MAX_GEN,
      replica' = {
        ...replica,
        generation: generation + seedBump,
        recoveredEpoch: acquiredTransitions,
        loadSucceeded: true,
      },
    },
    dag' = DRVS.mapBy(d => match pg.get(d) { | PgLive(a) => DagLive | PgNone => DagAbsent }),
    entries' = DRVS.mapBy(d => recoveredEntry(entries.get(d), pg.get(d), sealed.get(d))),
    // Every restamp (same- or cross-exec) clears the seal; a swept or
    // spared entry keeps it (the sweep skips sealed keys entirely).
    // Pre-699ea1692 the cross-exec arm kept the seal; pre-2f9a747d0 the
    // same-exec arm kept it.
    sealed' = DRVS.mapBy(d => match pg.get(d) {
      | PgLive(a) =>
        if (restampIsCrossExec(entries.get(d), a.exec))
          and { not(ENABLE_CROSS_EXEC_UNSEAL), sealed.get(d) }
        else
          and { not(ENABLE_SAME_EXEC_RESTAMP_CLEAR), sealed.get(d) }
      | PgNone => sealed.get(d)
    }),
    wit' = if (DRVS.exists(d => match pg.get(d) {
        | PgLive(a) => restampDropsLines(entries.get(d), a.exec)
        | PgNone => false
      }))
      { ...wit, crossExecCleared: true }
    else wit,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
  }

  /// `handle_leader_acquired`'s failed-load arm: the DAG load failed, the
  /// tenure continues with an EMPTY, NON-authoritative DAG, but
  /// `set_recovery_complete` still runs — the gate cannot wedge a
  /// degraded tenure, the periodic flusher still snapshots the retained
  /// buffers, and the disconnect-time discard fails closed
  /// (`dag_authoritative` stays false). No restamp and no sweep run; the
  /// prefix re-arm already did (it precedes the load).
  // Mirrors: handle_leader_acquired (the Err arm) + recover_from_pg's
  //          re-arm-then-fail path.
  action recoveryLoadFailed: bool = all {
    ENABLE_LOAD_FAILURE,
    isLeader,
    not(recoveryComplete),
    replica' = {
      ...replica,
      recoveredEpoch: acquiredTransitions,
      loadSucceeded: false,
    },
    dag' = DRVS.mapBy(d => DagAbsent),
    // The prefix re-arm runs before the load and therefore before the
    // failure: every retained entry's per-tenure latch is cleared.
    entries' = DRVS.mapBy(d => match entries.get(d) {
      | NoEntry => NoEntry
      | Entry(en) => Entry({ ...en, prefix: PrefUnchecked })
    }),
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// `handle_leader_acquired`'s TOCTOU-gate discard arm: the recovery ran
  /// to completion (re-arm, restamp, sweep — all of `recover_from_pg`'s
  /// entry mutations are already applied) but a lease flap, a rebound, or
  /// a missing post-claim confirmation landed before the gate, so the
  /// loaded DAG is discarded (`clear_persisted_state` again), the
  /// completion is NOT recorded, and the next LeaderAcquired re-runs
  /// recovery. The entry mutations are NOT rolled back — they are
  /// idempotent under the re-run.
  // Mirrors: handle_leader_acquired (the gate-discard arm).
  action recoveryDiscarded: bool = all {
    ENABLE_DISCARDED_RECOVERY,
    isLeader,
    not(recoveryComplete),
    replica' = { ...replica, loadSucceeded: false },
    dag' = DRVS.mapBy(d => DagAbsent),
    entries' = DRVS.mapBy(d => recoveredEntry(entries.get(d), pg.get(d), sealed.get(d))),
    // The same per-arm seal-clear switches as recoveryOk's.
    sealed' = DRVS.mapBy(d => match pg.get(d) {
      | PgLive(a) =>
        if (restampIsCrossExec(entries.get(d), a.exec))
          and { not(ENABLE_CROSS_EXEC_UNSEAL), sealed.get(d) }
        else
          and { not(ENABLE_SAME_EXEC_RESTAMP_CLEAR), sealed.get(d) }
      | PgNone => sealed.get(d)
    }),
    wit' = if (DRVS.exists(d => match pg.get(d) {
        | PgLive(a) => restampDropsLines(entries.get(d), a.exec)
        | PgNone => false
      }))
      { ...wit, crossExecCleared: true }
    else wit,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    leaseTransitions' = leaseTransitions,
  }

  // --- 6h. the interim leader's PG-visible effects ----------------------
  //
  // Model A is single-replica: the interim leader that holds the lease
  // between our lose and our re-acquire is not modeled as a process, only
  // as the mutations it leaves in the shared store for our next recovery
  // and our next flush to observe. Both actions are enabled only while
  // this replica has observed its loss (`!isLeader`) — the dual-belief
  // window in which the interim's store writes race ours is model B's
  // subject (cross-replica finalization).

  /// An interim leader observes the live execution's terminal, finalizes
  /// its `drv_logs` row, and persists the terminal status. The recorded
  /// span's end is at least one past every line ever delivered to ANY
  /// replica for that execution: the worker delivers every line before
  /// its completion report on the same ordered stream, and the interim's
  /// own flush obeys the gap-span rule, so a finalizing tenure's row
  /// covers the execution's true end (the lines only this replica
  /// received are below that end and therefore inside the finalized
  /// record's claimed-or-disclosed span). The end may exceed our
  /// delivered set (the interim received more lines than we did).
  /// Guarded on the row not already being finalized: the interim is
  /// post-fix (obs.log.finalize-immutable) — its own `upsert_drv_log`
  /// goes through the same `WHERE NOT drv_logs.is_complete` chokepoint
  /// as ours, so it cannot rewrite (not even extend) a row some tenure
  /// already finalized. Without this guard the abstract interim is
  /// strictly more powerful than any real post-fix replica and
  /// `finalizedRowFrozen` is falsified by an environment action no code
  /// path can perform.
  // Mirrors: another replica's terminal_log_epilogue + flush_final +
  //          upsert_drv_log, projected onto the shared row.
  action interimFinalizes(d: Drv): bool = all {
    not(isLeader),
    leaseTransitions > acquiredTransitions,
    pg.get(d) != PgNone,
    val e = match pg.get(d) { | PgNone => 1 | PgLive(a) => a.exec }
    // The end is at least every line this replica received, AND at least
    // the execution's current stored end: the interim's own final folds
    // its own stored coverage (stored-coverage-preserved), so its
    // finalized span can never regress what any tenure already recorded.
    val floor = maxInt(maxInt(setEnd(delivered.get(d).get(e)), 1), rowEnd(d, e))
    all {
      not(rowComplete(d, e)),
      floor <= MAX_LINE,
      nondet end = floor.to(MAX_LINE).oneOf()
      val newRow = { lo: 0, cnt: end, trueEnd: end, complete: true }
      all {
        stored' = stored.put(d, stored.get(d).put(e, Stored(newRow))),
        storedHist' = storedHist.put(d, storedHist.get(d).put(e,
          histAfterWrite(storedHist.get(d).get(e), newRow))),
      },
    },
    pg' = pg.put(d, PgNone),
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    delivered' = delivered,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// An interim leader's periodic flush extends the live execution's
  /// `drv_logs` row WITHOUT finalizing it: the interim received lines
  /// this replica may never have held (the worker reconnected to it and
  /// kept streaming) and snapshotted them, folding any prior stored
  /// coverage per the gap-merge rules — so the row stays exact
  /// (`lo + cnt == trueEnd`) and its end only grows. This is the
  /// documented reason `reconcile_stored_prefix` exists ("an interim
  /// leader flushed lines this replica never received … or stored
  /// coverage past the ring's tail") and the only way a NON-finalized
  /// stored row can end past this replica's retained ring once eviction
  /// is push-coupled: a row this replica writes always ends at its own
  /// ring's back + 1, so its own reconcile can never truncate the ring to
  /// empty. Without this action the recovered-prefix fold's hard case —
  /// the truncate-to-empty that used to reset the ingestion gate — is
  /// unreachable and `lineSpanExact`'s gap-merge branch is vacuous.
  // Mirrors: another replica's flush_periodic + upload_and_record +
  //          upsert_drv_log (is_final=false), projected onto the shared
  //          row.
  action interimExtendsRow(d: Drv): bool = all {
    not(isLeader),
    leaseTransitions > acquiredTransitions,
    pg.get(d) != PgNone,
    val e = match pg.get(d) { | PgNone => 1 | PgLive(a) => a.exec }
    all {
      // Only extend a live, non-finalized row (the frozen-row UPSERT
      // refuses writes to a finalized one), and strictly: a non-extending
      // snapshot is a stored-state no-op.
      not(rowComplete(d, e)),
      rowEnd(d, e) < MAX_LINE,
      nondet end = (rowEnd(d, e) + 1).to(MAX_LINE).oneOf()
      val newRow = { lo: 0, cnt: end, trueEnd: end, complete: false }
      all {
        stored' = stored.put(d, stored.get(d).put(e, Stored(newRow))),
        storedHist' = storedHist.put(d, storedHist.get(d).put(e,
          histAfterWrite(storedHist.get(d).get(e), newRow))),
      },
    },
    pg' = pg,
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    delivered' = delivered,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  /// An interim leader re-dispatches the derivation under a fresh
  /// execution (its worker disconnected or failed transiently): the
  /// persisted assignment now names an exec_id this replica has never
  /// seen, so our next recovery's restamp is a CROSS-exec restamp that
  /// must clear the retained lines.
  // Mirrors: another replica's assign_to_worker, projected onto the
  //          assignments row.
  action interimRedispatches(d: Drv, ex: Executor): bool = all {
    not(isLeader),
    leaseTransitions > acquiredTransitions,
    pg.get(d) != PgNone,
    dispatched.get(d) < MAX_EXECS,
    val nextExec = dispatched.get(d) + 1
    all {
      dispatched' = dispatched.put(d, nextExec),
      pg' = pg.put(d, PgLive({ exec: nextExec, executor: ex })),
    },
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dag' = dag,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  // --- 6i. executor disconnect ------------------------------------------

  /// `handle_executor_disconnected`'s log-buffer cleanup: for each path
  /// the disconnecting stream pushed accepted batches for, discard the
  /// entry iff the DAG has never heard of the path AND the entry is
  /// stamped to the disconnecting executor AND it is not sealed
  /// (`discard_if_owned_by`). Gated on `is_leader && dag_authoritative`:
  /// the DAG gate infers "stale" from "not in the DAG", which is only
  /// meaningful when the DAG reflects PG — a deposed leader, a
  /// pre-recovery window, or a failed-load tenure must NOT treat "not in
  /// the (empty) DAG" as "stale" and destroy the retained in-flight
  /// entries the lease-flap retention preserves. The model's `seen_drvs`
  /// is every derivation currently stamped to the executor — a superset
  /// of the real per-stream set, filtered identically by the ownership
  /// and DAG gates.
  // Mirrors: handle_executor_disconnected (the seen_drvs loop) +
  //          LogBuffers::discard_if_owned_by.
  action executorDisconnects(ex: Executor): bool = all {
    // Pre-2c301438d the discard loop ran unconditionally: "not in the
    // DAG" was read as "stale" even on a deposed leader whose DAG was
    // just wiped, destroying the retained in-flight entries the
    // lease-flap retention preserves.
    val gateOpen = or {
      not(ENABLE_DISCONNECT_DAG_GATE),
      isLeader and replica.loadSucceeded,
    }
    def reaped(d: Drv): bool = and {
      gateOpen,
      dag.get(d) == DagAbsent,
      hasEntry(d),
      entryExecutor(d) == ex,
      not(sealed.get(d)),
    }
    entries' = DRVS.mapBy(d => if (reaped(d)) NoEntry else entries.get(d)),
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    storedHist' = storedHist,
    replica' = replica,
    leaseTransitions' = leaseTransitions,
    wit' = wit,
  }

  // --- 8. init ----------------------------------------------------------

  /// The shared zero of both init shapes: no entries, no executions, no
  /// rows, all witness flags down.
  action initCommon = all {
    entries' = DRVS.mapBy(d => NoEntry),
    sealed' = DRVS.mapBy(d => false),
    pendingFinal' = DRVS.mapBy(d => NoFinal),
    dispatched' = DRVS.mapBy(d => 0),
    dag' = DRVS.mapBy(d => DagAbsent),
    pg' = DRVS.mapBy(d => PgNone),
    delivered' = DRVS.mapBy(d => 1.to(MAX_EXECS).mapBy(e => Set())),
    stored' = DRVS.mapBy(d => 1.to(MAX_EXECS).mapBy(e => NoRow)),
    storedHist' = DRVS.mapBy(d => 1.to(MAX_EXECS).mapBy(e => { frozen: NoRow, everEnd: 0 })),
    leaseTransitions' = 0,
    wit' = {
      evicted: false,
      rejectedNonMonotone: false,
      crossExecCleared: false,
      tenureDropReap: false,
      refusedUpsertReap: false,
      sealedEmptyReap: false,
      enqueueFailReap: false,
      genPinnedAcrossHolderChange: false,
      alreadyFinalizedRefusal: false,
    },
  }

  /// The `LeaderState::always_leader()` shape (the non-K8s single-replica
  /// deployment, and the post-first-acquire steady state of the K8s one):
  /// leader at epoch 0, generation 1, recovery already complete, DAG
  /// authoritative. Every shipped regime starts here; the K8s cold-start
  /// (standby → first acquire → first recovery) is covered by the flap
  /// regime's lose/re-acquire cycle, which exercises the same acquire-edge
  /// code paths against a non-trivial retained state (strictly harder).
  action init = all {
    initCommon,
    replica' = {
      isLeader: true,
      generation: 1,
      acquiredTransitions: 0,
      recoveredEpoch: 0,
      loadSucceeded: true,
    },
  }

  /// The K8s cold-start shape: a standby that has never held the lease.
  /// Not the `--main` init of any shipped regime (mkQuintCheck does not
  /// pass `--init`, and the flap regime's lose/re-acquire cycle covers the
  /// acquire edge against richer state); kept for REPL probing and for a
  /// future cold-start regime.
  action initPending = all {
    initCommon,
    replica' = {
      isLeader: false,
      generation: 1,
      acquiredTransitions: 0,
      recoveredEpoch: NO_EPOCH,
      loadSucceeded: false,
    },
  }

  // --- 9. step ----------------------------------------------------------
  // Every disjunct of `step` is a NAMED action with its nondet choices
  // bound directly inside it (the MBT tracker records the innermost
  // disjunct's name; an anonymous block makes the whole step anonymous
  // and undrivable). The flattening is semantics-preserving: `nondet x;
  // any { a(x), b(x) }` and `any { nondet x; a(x), nondet x; b(x) }` are
  // the same existential over the same (action, parameter) pairs.

  /// One derivation is dispatched to one executor.
  action dispatchAny = {
    nondet d = DRVS.oneOf()
    nondet ex = EXECUTORS.oneOf()
    dispatch(d, ex)
  }
  /// One worker batch is accepted into one entry (possibly evicting a
  /// head prefix of the appended ring — the push-coupled byte-cap
  /// eviction).
  action pushBatchAcceptedAny = {
    nondet d = DRVS.oneOf()
    nondet ex = EXECUTORS.oneOf()
    nondet lo = 0.to(MAX_LINE - 1).oneOf()
    nondet hi = 1.to(MAX_LINE).oneOf()
    pushBatchAccepted(d, ex, lo, hi)
  }
  /// One worker batch is rejected by the line-numbering gate.
  action pushBatchRejectedNonMonotoneAny = {
    nondet d = DRVS.oneOf()
    nondet ex = EXECUTORS.oneOf()
    nondet lo = 0.to(MAX_LINE - 1).oneOf()
    pushBatchRejectedNonMonotone(d, ex, lo)
  }
  /// One derivation reaches a terminal state and its final flush is
  /// enqueued.
  action terminalEnqueuedAny = {
    nondet d = DRVS.oneOf()
    terminalEnqueued(d)
  }
  /// One derivation reaches a terminal state and its final flush cannot
  /// be enqueued.
  action terminalEnqueueFailedAny = {
    nondet d = DRVS.oneOf()
    terminalEnqueueFailed(d)
  }
  /// One derivation reaches a terminal state whose PG persist fails.
  action terminalPersistFailedAny = {
    nondet d = DRVS.oneOf()
    terminalPersistFailed(d)
  }
  /// One terminal derivation's build cleanup fires.
  action cleanupTerminalAny = {
    nondet d = DRVS.oneOf()
    cleanupTerminal(d)
  }
  /// One entry's stored-coverage reconciliation settles.
  action reconcileStoredPrefixAny = {
    nondet d = DRVS.oneOf()
    reconcileStoredPrefix(d)
  }
  /// One entry's periodic snapshot uploads (or is refused).
  action flushPeriodicUploadAny = {
    nondet d = DRVS.oneOf()
    flushPeriodicUpload(d)
  }
  /// One sealed empty entry is reaped by the periodic tick.
  action flushPeriodicSealedEmptyReapAny = {
    nondet d = DRVS.oneOf()
    flushPeriodicSealedEmptyReap(d)
  }
  /// One pending final is dropped as out-of-tenure.
  action flushFinalDropStaleAny = {
    nondet d = DRVS.oneOf()
    flushFinalDropStale(d)
  }
  /// One pending final is refused because the row is already finalized.
  action flushFinalRefuseFinalizedAny = {
    nondet d = DRVS.oneOf()
    flushFinalRefuseFinalized(d)
  }
  /// One pending final's guard SELECT fails and the request defers.
  action flushFinalGuardErrorAny = {
    nondet d = DRVS.oneOf()
    flushFinalGuardError(d)
  }
  /// One pending final drains and uploads.
  action flushFinalUploadAny = {
    nondet d = DRVS.oneOf()
    flushFinalUpload(d)
  }
  /// One pending final drains an empty entry.
  action flushFinalEmptyDrainAny = {
    nondet d = DRVS.oneOf()
    flushFinalEmptyDrain(d)
  }
  /// One pending final finds its entry gone or restamped.
  action flushFinalStaleOrMissingAny = {
    nondet d = DRVS.oneOf()
    flushFinalStaleOrMissing(d)
  }
  /// One interim leader finalizes one execution's row.
  action interimFinalizesAny = {
    nondet d = DRVS.oneOf()
    interimFinalizes(d)
  }
  /// One interim leader's periodic flush extends one execution's row.
  action interimExtendsRowAny = {
    nondet d = DRVS.oneOf()
    interimExtendsRow(d)
  }
  /// One interim leader re-dispatches one derivation.
  action interimRedispatchesAny = {
    nondet d = DRVS.oneOf()
    nondet ex = EXECUTORS.oneOf()
    interimRedispatches(d, ex)
  }
  /// One executor's stream disconnects.
  action executorDisconnectsAny = {
    nondet ex = EXECUTORS.oneOf()
    executorDisconnects(ex)
  }

  action step = any {
    dispatchAny,
    pushBatchAcceptedAny,
    pushBatchRejectedNonMonotoneAny,
    terminalEnqueuedAny,
    terminalEnqueueFailedAny,
    terminalPersistFailedAny,
    cleanupTerminalAny,
    reconcileStoredPrefixAny,
    flushPeriodicUploadAny,
    flushPeriodicSealedEmptyReapAny,
    flushFinalDropStaleAny,
    flushFinalRefuseFinalizedAny,
    flushFinalGuardErrorAny,
    flushFinalUploadAny,
    flushFinalEmptyDrainAny,
    flushFinalStaleOrMissingAny,
    foreignSteal,
    loseLease,
    reacquireAfterHolderChange,
    reacquireSameEpoch,
    reboundLease,
    recoveryOk,
    recoveryLoadFailed,
    recoveryDiscarded,
    interimFinalizesAny,
    interimExtendsRowAny,
    interimRedispatchesAny,
    executorDisconnectsAny,
  }
}

// =========================================================================
// THE BASE REGIME: no lease transitions, no faults, no evictions. The
// replica holds the lease for the whole trace with its recovery complete;
// the only adversaries are the worker (batch ordering, gaps, foreign
// executors) and the unbiased scheduling of the flusher against the
// actor (a periodic tick can race a queued final). A regression in the
// core entry lifecycle — the binding gate, the span arithmetic, the
// seal/drain ordering, the sealed-empty reap — surfaces in this small
// fast check instead of buried in the lease-transition regimes.
//
// Constants are chosen for COVERAGE — the smallest state space that
// still reaches every interleaving class the invariants are about:
//   - 1 derivation: entries for distinct derivations share no state
//     except the lease and the flusher loop (neither of which is
//     contended at these bounds — the deferred-retry cap is 64 and the
//     flush channel 1000-deep), so a second derivation squares the state
//     space without adding an interleaving class. The binding gate's
//     foreign-executor rejection needs a foreign EXECUTOR, not a second
//     derivation.
//   - 2 executors: the bound one and the foreign one.
//   - 2 executions: the re-dispatch, the stale-request guard, and the
//     cross-exec restamp all need a second execution of the same
//     derivation.
//   - 3 line numbers: an interior hole ({0,2}), a head eviction
//     ({0,1}→{1}), and a gap-merge fold (prefix [0,1) + ring {2}) are
//     the smallest shapes that exercise the span arithmetic's three
//     branches.
//
// NON-VACUITY at these constants (machine-checked by the
// quint-log-witness-* checks in nix/quint.nix): a row is completed
// (noCompletedRow violated — noSilentLineLoss and lineSpanExact are not
// vacuous), a non-monotone batch is rejected, and the sealed-empty reap
// fires.
// =========================================================================
module logBufferLifecycleBase {
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
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).*

  // --- deterministic protocol runs --------------------------------------
  // Executable sanity checks of the happy path and the known-critical
  // scenarios; `quint test --main=logBufferLifecycleBase --match=<run>`
  // replays them. They do not change the verified state space (runs are
  // not part of `step`).

  /// The happy path: dispatch, stream two lines, terminal, final flush.
  /// The row records the full contiguous span at `is_complete = true` and
  /// the entry is gone.
  run happyPathRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 2))
      .then(terminalEnqueued("d1"))
      .then(reconcileStoredPrefix("d1"))
      .then(flushFinalUpload("d1"))
      .expect(all {
        entries.get("d1") == NoEntry,
        not(sealed.get("d1")),
        pendingFinal.get("d1") == NoFinal,
        rowComplete("d1", 1),
        rowLo("d1", 1) == 0,
        rowCnt("d1", 1) == 2,
      })

  /// The binding gate: a batch from the executor NOT bound to the entry
  /// is rejected (the accept action is disabled), and the foreign
  /// executor cannot pollute the ring.
  run bindingGateRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 1))
      .expect(all {
        not(batchAccepted("d1", "e2", 1)),
        batchAccepted("d1", "e1", 1),
      })

  /// An interior hole: the worker skips line 1 (delivered only to an
  /// interim leader in production). The final row's recorded span counts
  /// the hole (line_count = 3 over a 2-line blob is unrepresentable here
  /// — the model has no physical blob — but the recorded count is the
  /// line-number span, not the held-set size).
  run interiorHoleRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 1))
      .then(pushBatchAccepted("d1", "e1", 2, 3))
      .then(terminalEnqueued("d1"))
      .then(reconcileStoredPrefix("d1"))
      .then(flushFinalUpload("d1"))
      .expect(all {
        rowCnt("d1", 1) == 3,
        rowLo("d1", 1) == 0,
        rowComplete("d1", 1),
      })

  /// The non-monotone rejection: a batch at or below the ring's high
  /// water is dropped and the ring is unchanged.
  run nonMonotoneRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 2))
      .then(pushBatchRejectedNonMonotone("d1", "e1", 1))
      .expect(all {
        entryHeld("d1") == Set(0, 1),
        wit.rejectedNonMonotone,
      })
}

// =========================================================================
// THE LEASE-FLAP REGIME: lease transitions enabled (foreign steals,
// lose/re-acquire edges, same-epoch re-acquires, rebounds), recovery
// re-runs at every acquire edge, and the interim leader's PG-visible
// effects (finalizing an execution's row, re-dispatching a derivation
// under a fresh exec) are injected while this replica is deposed. No
// local faults — the PG persists succeed, the flush channel never fills,
// the guard SELECTs answer.
//
// MAX_TRANSITIONS = 2 admits exactly one full A→B→A holder-change cycle
// (foreign steal to count 1, our re-steal to count 2) plus the
// same-epoch re-acquire and the rebound shapes; the saturated-generation
// state (a holder change whose generation fetch_max is a no-op) is
// reachable through the recovery floor seed of an earlier same-epoch
// tenure (see recoveryOk's seedBump). MAX_GEN = 4 covers the seeded
// generation at these transition counts.
//
// This is the regime where the deferred-final tenure pin, the cross-exec
// restamp, the acquisition sweep, the stored-coverage reconciliation,
// the tenure-orphan reap, and the refused-UPSERT reap are all
// load-bearing; the witness checks pin each as reachable.
// =========================================================================
module logBufferLifecycleFlap {
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
    ENABLE_OVERLAP_FOLD = true
  ).*

  /// Regression run for the obs.log.gap-span+2 falsification: the
  /// reachable pre-fix counterexample trace, replayed against the gated
  /// model. An interim leader extends the execution's stored row past
  /// everything this replica's retained ring holds; the re-acquiring
  /// replica's stored-coverage reconcile fetches the row, truncates the
  /// whole ring away, and caches the prefix; the worker then re-delivers
  /// a line the stored row already covers. Pre-fix that line landed in
  /// the empty ring and the final's gap-merge fold double-counted it
  /// (line_count = prefix(3) + gap(0) + ring_span(1) = 4 over a true
  /// span of 3). With the accounted-floor gate the re-delivery is
  /// rejected, the final drains an empty ring, and the recorded span
  /// stays exact. The same trace against the Ungated module (below)
  /// still reaches the violation — the machine-checked evidence that the
  /// gate is the load-bearing fix.
  run gapSpanRedeliveryRegressionRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 2))
      .then(reconcileStoredPrefix("d1"))
      .then(flushPeriodicUpload("d1"))
      .expect(rowLo("d1", 1) == 0 and rowCnt("d1", 1) == 2 and not(rowComplete("d1", 1)))
      .then(loseLease)
      .then(foreignSteal)
      .then(interimExtendsRow("d1"))
      .expect(rowEnd("d1", 1) == 3 and not(rowComplete("d1", 1)))
      .then(reacquireAfterHolderChange)
      .then(recoveryOk)
      .expect(entryHeld("d1") == Set(0, 1) and entryPrefix("d1") == PrefUnchecked)
      .then(reconcileStoredPrefix("d1"))
      .expect(all {
        entryHeld("d1") == Set(),
        entryPrefix("d1") == PrefCached({ lo: 0, cnt: 3 }),
        entryAccountedBelow("d1") == 3,
        // The re-delivery of a covered line is no longer accepted.
        not(batchAccepted("d1", "e1", 1)),
      })
      .then(pushBatchRejectedNonMonotone("d1", "e1", 1))
      .expect(entryHeld("d1") == Set() and wit.rejectedNonMonotone)
      .then(terminalEnqueued("d1"))
      .then(flushFinalEmptyDrain("d1"))
      .expect(all {
        // The empty drain leaves the interim's exact row untouched
        // (is_complete stays false; the incomplete indicator survives).
        rowLo("d1", 1) == 0,
        rowCnt("d1", 1) == 3,
        not(rowComplete("d1", 1)),
        lineSpanExact,
      })
}

// =========================================================================
// THE FAULT REGIMES: the local fault-injection branches enabled on top of
// the flap regime's lease budget, split into four checks by fault class
// (local = eviction + enqueue failure; recovery = load failure +
// discarded recovery; persist; guard). The single all-faults product
// regime converges but multiplies the flap regime's distinct-state count
// by every fault class's branching factor at once, blowing past the
// per-check state/wall-clock budget — as did the recovery+persist+guard
// and persist+guard intermediate groupings; the four-way split keeps
// each check within the same order as the largest checks already in the
// gate while every fault switch (and therefore every fault-dependent
// reap path and witness) remains exercised against the full lease budget
// in exactly one of the four. The cost of the split is the loss of
// cross-fault-class interactions (an eviction interleaved with a load
// failure, a persist failure interleaved with a guard failure); no
// invariant or documented residual depends on such an interaction beyond
// the persist x guard correlation, which is pinned by the rio-scheduler
// deferred-final unit tests (see logBufferLifecycleFaultGuard's header);
// a newly suspected interaction gets its own targeted regime rather than
// a return to the product.
// =========================================================================

// The LOCAL fault regime: ring evictions (the push-coupled byte-cap
// eviction) and flush-channel-full enqueue failures. The lease budget is
// retained because the eviction's interaction with the stored-coverage
// reconciliation (an evicted head makes the ring stop subsuming the row
// this replica itself wrote, forcing the fold branch) is only reachable
// across a re-acquisition. This is the regime where the eviction's
// head-loss disclosure and the enqueue-failure reap are load-bearing.
module logBufferLifecycleFaultLocal {
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
  ).*
}

// The RECOVERY fault regime: DAG-load failures and TOCTOU-discarded
// recoveries. The lease budget is retained because the recovery faults
// are only reachable after an acquire edge and their interesting
// interleavings are with the interim leader's PG mutations (a failed
// load retaining a cross-exec-stale entry until the next successful
// recovery clears it). This is the regime where the degraded
// (non-authoritative-DAG) tenure's retain-everything posture is
// load-bearing.
module logBufferLifecycleFaultRecovery {
  import logBufferLifecycle(
    DRVS = Set("d1"),
    EXECUTORS = Set("e1", "e2"),
    MAX_EXECS = 2,
    MAX_LINE = 3,
    MAX_TRANSITIONS = 2,
    MAX_GEN = 4,
    ENABLE_EVICTION = false,
    ENABLE_LOAD_FAILURE = true,
    ENABLE_DISCARDED_RECOVERY = true,
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
    ENABLE_OVERLAP_FOLD = true
  ).*
}

// The TERMINAL-PERSIST fault regime: the terminal-status persist
// failure — the correlated failure that leaves PG saying a terminal
// derivation is still running. The lease budget is retained because the
// fault's payoff is what the NEXT tenure does with the stale
// assignment: reload and same-exec-restamp the sealed entry as the live
// carrier, let an interim leader finalize the still-live execution
// behind this replica's seal (the refused-UPSERT reap's trigger), or
// let an interim re-dispatch it (the only path to a sealed entry
// meeting a cross-exec restamp — the seal-clearing that arm performs is
// load-bearing exactly here). This fault unlocks the entire
// post-terminal interim subtree (every interim action requires a live
// assignment, and a successful terminal persist removes it), which is
// why it gets its own regime.
//
// MAX_LINE is 2, not 3: this regime's subject is the entry / seal /
// assignment lifecycle under a stale persisted terminal, which is
// line-count-independent — 2 lines suffice for a non-empty ring, a
// non-empty delivered set, a row with a non-trivial span, and every
// reap predicate. The line-arithmetic shapes that need 3 lines (the
// interior hole, the gap-merge fold with a non-zero gap, the head
// eviction) are the base, flap, and fault-local regimes' subject. At
// MAX_LINE = 3 the post-terminal interim subtree this fault unlocks
// pushes the regime several times past the per-check budget without
// adding an interleaving class any of its invariants or witnesses
// depend on.
module logBufferLifecycleFaultPersist {
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
    ENABLE_FINALIZE_REFUSAL = true,
    ENABLE_TRUE_GAP_SPAN = true,
    ENABLE_RING_SPAN = true,
    ENABLE_OVERLAP_FOLD = true
  ).*
}

// The FINALIZE-GUARD fault regime: the finalize-guard SELECT failure
// (the deferral path — the flusher cannot rule out an existing
// finalization, so the final is retained and retried instead of
// uploaded). The lease budget is retained because the deferral is only
// interesting against the tenure pin (a request deferred under one
// tenure must be dropped, not retried, once the tenure ends). This is
// the regime where the deferred-final retention is load-bearing. The
// persist-failure x guard-failure product (the same PG outage causing
// both) is deliberately not explored as a single regime — it multiplies
// the two faults' subtrees into a state space several times the
// per-check budget; the correlated scenario's specific interleaving
// (persist fails, the final defers, the lease lapses, the next tenure
// restamps the same exec, the deferred final must be dropped) is pinned
// by the rio-scheduler deferred-final unit tests cited at
// obs.log.deferred-final-retry.
module logBufferLifecycleFaultGuard {
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
    ENABLE_GUARD_FAILURE = true,
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
    ENABLE_OVERLAP_FOLD = true
  ).*
}

// =========================================================================
// THE UNGATED VARIANT — the expect-violation witness that the accept
// gate is the load-bearing fix for the obs.log.gap-span+2 falsification,
// and calibration-table entry #0.
//
// Identical to the flap regime except ENABLE_ACCOUNTED_FLOOR = false:
// the batch accept predicate degrades to the pre-fix comparison against
// the ring's CURRENT tail only, which resets when the stored-coverage
// reconcile truncates the ring to empty. lineSpanExact is then VIOLATED:
// an interim leader extends the execution's stored row past everything
// this replica's retained ring holds, the re-acquiring replica's
// reconcile fetches the row, truncates the whole ring away, and caches
// the prefix; a re-delivered (or fabricated) line the stored row already
// covers lands in the empty ring; and the gap-merge fold saturates the
// negative prefix-to-ring gap to 0 and adds the overlapping ring span on
// top of the prefix span that already covers it. The recorded
// first_line + line_count overshoots the execution's true end by the
// overlap.
//
// Wired as quint-log-witness-gap-span-ungated in nix/quint.nix: the
// check PASSES only when the checker violates lineSpanExact here. A
// green exhaustive check on the gated regimes plus a red lineSpanExact
// here is the machine-checked statement "the gate is necessary and
// sufficient at these bounds". The deterministic run below pins the
// minimal trace; the analysis lives in the campaign's
// phase2-falsification-lineSpanExact report and its adversarial review
// (which corrected the original reproducer's unreachable free-standing
// eviction step to the interim-leader row extension used here).
// =========================================================================
module logBufferLifecycleUngated {
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
    ENABLE_ACCOUNTED_FLOOR = false,
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
    ENABLE_OVERLAP_FOLD = true
  ).*

  /// The minimal reachable pre-fix counterexample. Steps 1-9 are
  /// identical to the flap regime's gapSpanRedeliveryRegressionRun; the
  /// re-delivery at step 10 is ACCEPTED here (the empty ring has no
  /// surviving high-water mark) and the final fold double-counts it.
  run ungatedRedeliveryFalsifiesSpanRun =
    init
      .then(dispatch("d1", "e1"))
      .then(pushBatchAccepted("d1", "e1", 0, 2))
      .then(reconcileStoredPrefix("d1"))
      .then(flushPeriodicUpload("d1"))
      .expect(rowLo("d1", 1) == 0 and rowCnt("d1", 1) == 2 and not(rowComplete("d1", 1)))
      .then(loseLease)
      .then(foreignSteal)
      .then(interimExtendsRow("d1"))
      .expect(rowEnd("d1", 1) == 3 and not(rowComplete("d1", 1)))
      .then(reacquireAfterHolderChange)
      .then(recoveryOk)
      .expect(entryHeld("d1") == Set(0, 1) and entryPrefix("d1") == PrefUnchecked)
      .then(reconcileStoredPrefix("d1"))
      .expect(entryHeld("d1") == Set() and entryPrefix("d1") == PrefCached({ lo: 0, cnt: 3 }))
      // The worker re-delivers line 1 — a line the cached prefix already
      // covers. The ring is empty, so the pre-fix tail comparison has
      // nothing to compare against and the batch is accepted.
      .then(pushBatchAccepted("d1", "e1", 1, 2))
      .expect(entryHeld("d1") == Set(1))
      .then(terminalEnqueued("d1"))
      .then(flushFinalUpload("d1"))
      .expect(all {
        rowComplete("d1", 1),
        rowLo("d1", 1) == 0,
        // prefix(3) + saturated-gap(0) + ring_span(1) = 4 over a true
        // span of 3: one line past the execution's true end.
        rowCnt("d1", 1) == 4,
        not(lineSpanExact),
      })
}
