// -*- mode: Bluespec; -*-
//
// fencedWrites — the claims-floor fence RE-STATED OVER INTERLEAVED
// TRANSACTIONS (A1 fenced-write-discipline; bughunt wave).
//
// The gap this model closes: leaderElection.qnt proves generation
// distinctness and materializationJob.qnt proves job-table writes are
// fenced as ATOMIC actions — neither models the READ COMMITTED window
// between a transaction's floor read and its write, which is exactly
// where bug_261 (mint upsert TOCTOU), merged_bug_231 (derivation-keyed
// deposed close), bug_273 (plain-SET floor regression) and bug_393
// (terminal refusal answer) lived.
//
// READ COMMITTED is modeled precisely:
//   - beginTx snapshots the floor VISIBLE AT READ TIME
//     (TxFloorRead(v)); claims committed later are invisible to the
//     Rust-side comparison — the advisory half.
//   - the guarded upsert's in-statement predicate evaluates against
//     the row's LATEST COMMITTED version (EvalPlanQual), independent
//     of the snapshot — the authoritative half
//     (sched.lease.fence-statement-guard).
//
// Scope (Tier-1 exhaustive): 2 replicas, MAX_GEN = 3, ONE derivation's
// active assignments row, one persisted resource floor. The fixed
// system's actions only; the four pre-fix behaviors live in
// calibration overrides (calibration/fence-*.qnt), each the
// falsifiability pair of one invariant below.
//
// PRICED RESIDUAL (kept reachable, bounded): a fresh INSERT below the
// floor when NO active conflict row exists to evaluate against. It
// cannot regress any newer row by construction — activeRowGenMonotonic
// holds with the residual reachable (the fence-invariant-map.md
// "fresh-insert window" entry; production bound: the successor's own
// re-mint conflicts on the partial-unique index and the in-statement
// guard then governs).
//
// Run (exhaustive, all five invariants — the wired check):
//   quint verify --backend=tlc --main=fencedWritesT1 \
//     --invariant=fencedWritesAll docs/spec/models/fencedWrites.qnt
module fencedWrites {
  // --- 1. constants ---------------------------------------------------------

  /// Generation ceiling (claims stop at MAX_GEN — the exhaustive bound).
  const MAX_GEN: int
  /// Mint ceiling (execsMinted stops here — keeps the state space
  /// finite for TLC's exhaustive BFS).
  const MAX_EXECS: int
  /// Replica ids.
  const REPLICAS: Set[str]

  // --- 2. types --------------------------------------------------------------

  /// Per-replica transaction phase: Idle, or holding the floor value
  /// its begin-time read SAW (the READ COMMITTED snapshot half).
  type TxPhase = | TxIdle | TxFloorRead(int)

  // --- 3. state --------------------------------------------------------------

  /// Committed leader_generation_claims rows.
  var committedClaims: Set[int]
  /// The active assignments row's generation (0 = no active row).
  var activeGen: int
  /// The active row's owning replica ("" = none) — exec-scope carrier.
  var activeOwner: str
  /// The active row's exec id (0 = none); fresh ids from execsMinted.
  var activeExec: int
  var execsMinted: int
  /// The persisted per-derivation resource floor (bug_273's subject).
  var resourceFloor: int
  /// The generation each replica serves (its own claim; 0 = never led).
  var replicaGen: str -> int
  /// Per-replica tx phase.
  var txPhase: str -> TxPhase
  /// A fence refusal awaiting its client answer (bug_393's subject).
  var pendingRefusal: str -> bool

  // --- 4. latches (history properties as state invariants) -------------------

  /// An EXISTING active row's generation decreased (or its exec was
  /// clobbered by a lower-generation write) — the bug_261 regression.
  var rowRegressed: bool
  /// A transaction whose visible floor exceeded its replica's
  /// generation mutated decision state — the open-coded-floor-compare
  /// bypass class (set only by calibration overrides).
  var belowFloorMutation: bool
  /// The resource floor decreased (bug_273's plain-SET regression).
  var floorRegressed: bool
  /// A deposed replica closed an active row it does not own
  /// (merged_bug_231's derivation-keyed close).
  var deposedCloseApplied: bool
  /// A fence refusal was answered with a terminal code and the client
  /// gave up (bug_393).
  var clientGaveUp: bool

  // --- 5. helpers --------------------------------------------------------------

  pure def maxOf(s: Set[int]): int = s.fold(0, (a, x) => if (x > a) x else a)

  /// The durable claims floor: GREATEST over claims and the active
  /// assignment row — the same two arms as SchedulerDb::claims_floor.
  def currentFloor: int = maxOf(committedClaims.union(Set(activeGen)))

  /// Whether replica r is deposed (serves below the durable floor).
  def deposed(r: str): bool = replicaGen.get(r) < currentFloor

  // --- 6. frames -----------------------------------------------------------------

  action rowUnchanged = all {
    activeGen' = activeGen,
    activeOwner' = activeOwner,
    activeExec' = activeExec,
    execsMinted' = execsMinted,
  }
  action floorUnchanged = all { resourceFloor' = resourceFloor }
  action latchesUnchanged = all {
    rowRegressed' = rowRegressed,
    belowFloorMutation' = belowFloorMutation,
    floorRegressed' = floorRegressed,
    deposedCloseApplied' = deposedCloseApplied,
    clientGaveUp' = clientGaveUp,
  }
  action claimsUnchanged = all {
    committedClaims' = committedClaims,
    replicaGen' = replicaGen,
  }

  // --- 7. init ---------------------------------------------------------------------

  action init = all {
    committedClaims' = Set(1),
    activeGen' = 0,
    activeOwner' = "",
    activeExec' = 0,
    execsMinted' = 0,
    resourceFloor' = 1,
    // r1 starts as the generation-1 leader; everyone else has never led.
    replicaGen' = REPLICAS.mapBy(r => if (r == "r1") 1 else 0),
    txPhase' = REPLICAS.mapBy(r => TxIdle),
    pendingRefusal' = REPLICAS.mapBy(r => false),
    rowRegressed' = false,
    belowFloorMutation' = false,
    floorRegressed' = false,
    deposedCloseApplied' = false,
    clientGaveUp' = false,
  }

  // --- 8. actions --------------------------------------------------------------------

  /// A replica claims the next generation (the successor). The claim
  /// COMMITS immediately — every later floor READ sees it; an already
  /// in-flight transaction's snapshot does not (READ COMMITTED).
  action successorClaim(r: str): bool = all {
    maxOf(committedClaims) + 1 <= MAX_GEN,
    committedClaims' = committedClaims.union(Set(maxOf(committedClaims) + 1)),
    replicaGen' = replicaGen.set(r, maxOf(committedClaims) + 1),
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    rowUnchanged, floorUnchanged, latchesUnchanged,
  }

  /// begin_fenced's floor read: snapshot the floor VISIBLE NOW.
  action beginTx(r: str): bool = all {
    txPhase.get(r) == TxIdle,
    replicaGen.get(r) > 0,
    txPhase' = txPhase.set(r, TxFloorRead(currentFloor)),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
  }

  /// begin_fenced refuses: the visible floor exceeds the replica's
  /// generation — Fenced{floor}. The transaction ends having written
  /// NOTHING; the refusal awaits its client answer (bug_393's hook).
  action fenceRefuse(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v > replicaGen.get(r)
    },
    txPhase' = txPhase.set(r, TxIdle),
    pendingRefusal' = pendingRefusal.set(r, true),
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
  }

  /// The GUARDED mint upsert (sched.lease.fence-statement-guard): the
  /// advisory floor compare passed at read time; the in-statement
  /// predicate `assignments.generation <= EXCLUDED.generation`
  /// evaluates against the row's LATEST COMMITTED version
  /// (EvalPlanQual) — NOT the snapshot. Equality passes (same-epoch
  /// re-acquire keep). rows_affected == 0 ⇒ rollback + Fenced.
  action guardedUpsertCommit(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r) // the advisory half passed
    },
    execsMinted < MAX_EXECS,
    txPhase' = txPhase.set(r, TxIdle),
    val g = replicaGen.get(r)
    if (activeGen <= g) all {
      // Applies: fresh insert (activeGen == 0 — the priced
      // residual: g may sit below a claims-only floor) or guarded
      // update of an equal-or-older row. Never a regression.
      activeGen' = g,
      activeOwner' = r,
      activeExec' = execsMinted + 1,
      execsMinted' = execsMinted + 1,
      pendingRefusal' = pendingRefusal,
      latchesUnchanged,
    } else all {
      // Predicate refused against the newer committed row: zero
      // rows, rollback, surfaced as the same retryable refusal.
      rowUnchanged,
      pendingRefusal' = pendingRefusal.set(r, true),
      latchesUnchanged,
    },
    claimsUnchanged, floorUnchanged,
  }

  /// The exec-scoped fenced close (FencedTx::close_assignment): a
  /// replica closes only the row IT owns, inside an admitted
  /// transaction. Deposed replicas never reach here (their begin
  /// refuses); a non-owner's close affects zero rows.
  action fencedClose(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r)
    },
    txPhase' = txPhase.set(r, TxIdle),
    if (activeGen > 0 and activeOwner == r) all {
      activeGen' = 0,
      activeOwner' = "",
      activeExec' = 0,
      execsMinted' = execsMinted,
    } else {
      rowUnchanged
    },
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged, latchesUnchanged,
  }

  /// The fenced resource-floor write with the server-side per-
  /// dimension GREATEST ratchet (bug_273's fix): even a live leader's
  /// stale-base write can only raise the floor.
  action floorWriteGreatest(r: str, v: int): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(f) => f <= replicaGen.get(r)
    },
    txPhase' = txPhase.set(r, TxIdle),
    resourceFloor' = if (v > resourceFloor) v else resourceFloor,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, latchesUnchanged,
  }

  /// The client answers a fence refusal as RETRYABLE (UNAVAILABLE
  /// family — sched.grpc.fence-retryable): it retries; nobody gives
  /// up. The fixed system's only answer arm.
  action answerRetryable(r: str): bool = all {
    pendingRefusal.get(r),
    pendingRefusal' = pendingRefusal.set(r, false),
    txPhase' = txPhase,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
  }

  // --- 9. step ------------------------------------------------------------------------

  action step = {
    nondet r = REPLICAS.oneOf()
    nondet v = 1.to(MAX_GEN).oneOf()
    any {
      successorClaim(r),
      beginTx(r),
      fenceRefuse(r),
      guardedUpsertCommit(r),
      fencedClose(r),
      floorWriteGreatest(r, v),
      answerRetryable(r),
    }
  }

  // --- 10. invariants -------------------------------------------------------------------

  /// bug_261: an existing active row's generation never regresses and
  /// its exec is never clobbered by a lower-generation write.
  /// Falsifiability pair: calibration fence-261-unguarded-upsert.
  val activeRowGenMonotonic = not(rowRegressed)

  /// The open-coded-floor-compare bypass class: a transaction whose
  /// visible floor exceeded its generation never mutates decision
  /// state. Holds structurally in the fixed system (such a tx can
  /// only fenceRefuse); bites under the 231/273 calibration steps.
  val belowFloorTxNeverMutates = not(belowFloorMutation)

  /// bug_273: the resource floor is monotone.
  /// Falsifiability pair: calibration fence-273-plain-floor-set.
  val resourceFloorMonotonic = not(floorRegressed)

  /// merged_bug_231: a deposed replica never closes an active row it
  /// does not own.
  /// Falsifiability pair: calibration fence-231-unfenced-close.
  val openAttemptViewStableUnderDeposedClose = not(deposedCloseApplied)

  /// bug_393: every fence refusal is answered retryably — no client
  /// ever gives up on a request the live leader would accept.
  /// Falsifiability pair: calibration fence-393-terminal-refusal.
  val fenceRefusalAlwaysRetryable = not(clientGaveUp)

  /// The wired conjunction.
  val fencedWritesAll = and {
    activeRowGenMonotonic,
    belowFloorTxNeverMutates,
    resourceFloorMonotonic,
    openAttemptViewStableUnderDeposedClose,
    fenceRefusalAlwaysRetryable,
  }
}

/// Tier-1 exhaustive scope: 2 replicas, 3 generations, 1 drv.
module fencedWritesT1 {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2")
  ).*
}
