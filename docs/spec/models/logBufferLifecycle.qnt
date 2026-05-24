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
//   - the batch accept/reject predicate (seal, binding gate, monotone
//     numbering);
//   - the byte-cap eviction, abstracted to "an arbitrary non-empty proper
//     prefix of the held lines may be dropped" (a sound over-approximation
//     of the real eviction policy — the model reaches strictly more states
//     than the code, so a safety invariant that holds here holds there);
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
  type EntrySlot =
    | NoEntry
    | Entry({
        exec: int,
        executor: Executor,
        held: Set[int],
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

  // The lease-composition interface state (LeaderState's atomics).
  var isLeader: bool
  var generation: int
  var acquiredTransitions: int
  /// `recovery_completed_for`: the acquire-epoch stamp of the last
  /// completed recovery, or NO_EPOCH. `recovery_complete()` is
  /// `recoveredEpoch == acquiredTransitions`.
  var recoveredEpoch: int
  /// The actor's `dag_authoritative` bit: true only after a tenure's
  /// recovery LOADED the DAG successfully (and was not discarded).
  var loadSucceeded: bool
  /// The Lease object's global `leaseTransitions` count at the apiserver
  /// (holder changes so far, including foreign ones we have not observed).
  var leaseTransitions: int

  // History flags (model-only bookkeeping, never read by an action guard;
  // each is the discriminator for one expect-violation witness below).
  /// An eviction dropped a non-empty head prefix of a held ring.
  var wEvicted: bool
  /// The ingestion gate rejected a batch as non-monotone.
  var wRejectedNonMonotone: bool
  /// A cross-exec restamp cleared a non-empty ring.
  var wCrossExecCleared: bool
  /// `flush_final`'s out-of-tenure drop arm reaped a sealed empty entry.
  var wTenureDropReap: bool
  /// The periodic refused-UPSERT reap fired (frozen row ⇒ sealed orphan
  /// discarded).
  var wRefusedUpsertReap: bool
  /// The periodic sealed-empty reap fired (empty snapshot of a sealed
  /// entry).
  var wSealedEmptyReap: bool
  /// The terminal epilogue's enqueue-failure reap fired.
  var wEnqueueFailReap: bool
  /// A holder change was re-acquired with the generation unchanged (the
  /// saturated regime — the `req_in_tenure` counterexample's premise).
  var wGenPinnedAcrossHolderChange: bool

  // --- 5. derived state -------------------------------------------------

  /// `LeaderState::recovery_complete()`.
  def recoveryComplete: bool =
    recoveredEpoch != NO_EPOCH and recoveredEpoch == acquiredTransitions

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
  def reqInTenure(req: Req): bool = and {
    isLeader,
    generation == req.gen,
    acquiredTransitions == req.transitions,
  }

  /// The lines held by `d`'s entry iff it is stamped with `e`, else the
  /// empty set. The conservation law's "present in the in-memory ring"
  /// disjunct.
  def heldFor(d: Drv, e: int): Set[int] =
    match entries.get(d) {
      | NoEntry => Set()
      | Entry(en) => if (en.exec == e) en.held else Set()
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
    leaseTransitions' = 0,
    wEvicted' = false,
    wRejectedNonMonotone' = false,
    wCrossExecCleared' = false,
    wTenureDropReap' = false,
    wRefusedUpsertReap' = false,
    wSealedEmptyReap' = false,
    wEnqueueFailReap' = false,
    wGenPinnedAcrossHolderChange' = false,
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
    isLeader' = true,
    generation' = 1,
    acquiredTransitions' = 0,
    recoveredEpoch' = 0,
    loadSucceeded' = true,
  }

  /// The K8s cold-start shape: a standby that has never held the lease.
  /// Not the `--main` init of any shipped regime (mkQuintCheck does not
  /// pass `--init`, and the flap regime's lose/re-acquire cycle covers the
  /// acquire edge against richer state); kept for REPL probing and for a
  /// future cold-start regime.
  action initPending = all {
    initCommon,
    isLeader' = false,
    generation' = 1,
    acquiredTransitions' = 0,
    recoveredEpoch' = NO_EPOCH,
    loadSucceeded' = false,
  }

  // Placeholder transition relation: replaced by the real `step` as the
  // action tasks land. Keeps `quint run` meaningful from the first commit.
  action step = all {
    entries' = entries,
    sealed' = sealed,
    pendingFinal' = pendingFinal,
    dispatched' = dispatched,
    dag' = dag,
    pg' = pg,
    delivered' = delivered,
    stored' = stored,
    isLeader' = isLeader,
    generation' = generation,
    acquiredTransitions' = acquiredTransitions,
    recoveredEpoch' = recoveredEpoch,
    loadSucceeded' = loadSucceeded,
    leaseTransitions' = leaseTransitions,
    wEvicted' = wEvicted,
    wRejectedNonMonotone' = wRejectedNonMonotone,
    wCrossExecCleared' = wCrossExecCleared,
    wTenureDropReap' = wTenureDropReap,
    wRefusedUpsertReap' = wRefusedUpsertReap,
    wSealedEmptyReap' = wSealedEmptyReap,
    wEnqueueFailReap' = wEnqueueFailReap,
    wGenPinnedAcrossHolderChange' = wGenPinnedAcrossHolderChange,
  }
}

// =========================================================================
// Smoke-test instantiation: the base regime's constants. Replaced by the
// real regime modules in the regimes task; present from the first commit
// so `quint typecheck` and `quint run` have a concrete module to act on.
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
    ENABLE_GUARD_FAILURE = false
  ).*
}
