// -*- mode: Bluespec; -*-
// quint-policy-latches: rowRegressed, belowFloorMutation, floorRegressed, deposedCloseApplied, clientGaveUp, closeScopeViolated
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
// holds with the residual reachable (deliberately not excluded).
// Production bound: the successor's own re-mint conflicts on the
// partial-unique index, where the in-statement guard governs; the
// worst case is one transient dual delivery, settled by the
// report-side fence.
//
// TB-4 (bughunt-13 referee, SURVIVED): the direct closer was
// derivation-keyed — `fencedClose` closed the derivation's CURRENT
// row gated on OWNERSHIP (activeOwner == r), while its own doc cited
// `FencedTx::close_assignment`, whose production selector is
// `exec_id = $1`, NOT owner-gated (assignments carry no
// scheduler-replica owner; the fence is the only gate). Two faces:
// (a) under-constraint — an owner whose subject exec was superseded
// by its own re-mint closed the SUCCESSOR exec in-model, a
// destructive close production's stale-exec selector turns into zero
// rows, with NO latch watching the direct path; (b) over-constraint —
// the owner gate excluded the failover settle (a live successor
// closing a predecessor-minted row: the daily report/sweep path,
// housekeeping.rs + materialize.rs close by ids read from reports and
// DB rows whoever minted them). The close is now keyed on the exec
// the caller NAMES (`fencedCloseNamed`); the owner gate is gone; the
// closeScopeViolated latch + the fence-tb4-derivation-keyed-close
// calibration hold the law falsifiable; the noDirectCloseApplied /
// noForeignSettle witnesses prove both repaired regions are entered.
// Environment bound, priced at the action: production commit order
// (claim ≼ mint ≼ exec-id-visibility) makes a deposed replica unable
// to NAME an exec minted under a claim its floor read missed; the
// model encodes the bound as an enabling condition instead of
// data-flow provenance state.
//
// BUGHUNT-2 PLANES (slot 2): three axis-gated planes extend the same
// board — (1) tenure-stamp (merged_bug_338: writes carry the
// claim-stamped tenure, never a fresh lease-atomic read), (2) outbox
// replay (merged_bug_011: a latched batch re-derives before replay
// and closes only its latched exec), (3) tenure lifecycle (bug_155: a
// tenure completes only through the recovered-DAG witness; a failed
// recovery steps down). Each plane's actions are gated by an ENABLE_*
// const so the four legacy calibrations bind them FALSE and keep
// their state space identical; fencedWritesT1 binds all three TRUE
// (the live exhaustive check is the twins' baseline-hold pair). The
// plane latches are LIVE-COMPUTED inside shared parameterized apply
// sub-actions (the formal-policy P4/P5 conventions: oracles in the
// live module; twins perturb DECISION parameters only).
//
// Run (exhaustive, all eight invariants — the wired check):
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
  /// Plane 1 gate (merged_bug_338): enables the mid-tenure lease-atomic
  /// bump event. The legacy calibrations bind false.
  const ENABLE_ATOMIC_REREAD: bool
  /// Plane 2 gate (merged_bug_011): enables the outbox latch/advance/
  /// flush actions. The legacy calibrations bind false.
  const ENABLE_OUTBOX: bool
  /// Plane 3 gate (bug_155): enables the recovery/complete/step-down
  /// lifecycle actions. The legacy calibrations bind false.
  const ENABLE_TENURE_LIFECYCLE: bool

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
  //
  // bughunt-2 bug_358: every latch is computed LIVE by the oracle-seated
  // apply sub-actions in section 8a — never by a calibration override.
  // A calibration perturbs a DECISION (skips a guard, drops a ratchet,
  // flips an answer) and routes through the same seat; the oracle
  // observes the perturbed decision and latches by itself. The
  // quint-policy lint enforces this: P4 (every latch has a non-identity
  // writer reachable from this module's step) and P5 (no calibration
  // action assigns a latch).

  /// An EXISTING active row's generation decreased (or its exec was
  /// clobbered by a lower-generation write) — the bug_261 regression.
  /// Computed in upsertApply.
  var rowRegressed: bool
  /// A transaction whose believed floor (its snapshot; fenceless
  /// writers believe currentFloor) exceeded its replica's generation
  /// mutated decision state — the open-coded-floor-compare bypass
  /// class. Computed in every mutation seat via snapshotExceedsGen.
  var belowFloorMutation: bool
  /// The resource floor decreased (bug_273's plain-SET regression).
  /// Computed in floorWriteApply.
  var floorRegressed: bool
  /// A deposed replica closed an active row it does not own
  /// (merged_bug_231's derivation-keyed close). Computed in closeApply.
  var deposedCloseApplied: bool
  /// A fence refusal was answered with a terminal code and the client
  /// gave up (bug_393). Computed in answerRefusal.
  var clientGaveUp: bool
  /// A direct close landed on an exec OTHER than the one its caller
  /// NAMED (TB-4 — the derivation-/owner-keyed close class on the
  /// per-attempt path; production's `exec_id = $1` selector makes the
  /// mismatch a zero-row no-op). Computed in closeApply.
  var closeScopeViolated: bool
  /// Reachability ghost (TB-4 non-vacuity): some exec-matched direct
  /// close APPLIED — the re-keyed close arm is exercised, not
  /// vacuously green. Witness `noDirectCloseApplied` is wired
  /// expected-VIOLATED.
  var directCloseApplied: bool
  /// Reachability ghost (TB-4 non-vacuity): a LIVE non-owner closed
  /// the row it named — the failover settle the former owner gate
  /// excluded from the verified space. Witness `noForeignSettle` is
  /// wired expected-VIOLATED.
  var foreignSettleApplied: bool

  // --- 4b. bughunt-2 plane state ---------------------------------------------

  /// Plane 1: the live lease atomic per replica — can advance AHEAD of
  /// the claim-stamped tenure (replicaGen) via a mid-tenure
  /// re-acquire/rebound. The typed ServingGeneration is replicaGen;
  /// merged_bug_338 was stamping THIS instead.
  var atomicGen: str -> int
  /// Plane 1 latch (live-computed in mintApplyAs): a committed write
  /// carried a generation other than its replica's claim-stamped
  /// tenure.
  var staleWriteCommitted: bool
  /// Plane 2: the outbox-latched exec (0 = empty) and whether the
  /// derivation has ADVANCED past the latch (resubmit re-mint).
  var latchedExec: int
  var latchedStale: bool
  /// Plane 2 latches (live-computed in flushReplayApply): a stale
  /// batch was applied; a replay close hit a non-latched exec.
  var staleReplayApplied: bool
  var successorExecClosed: bool
  /// Plane 3: per-replica tenure lifecycle — the recovered-DAG witness
  /// and the completion stamp. Reset on every new claim (new tenure).
  var recoveredOk: str -> bool
  var tenureComplete: str -> bool
  /// Plane 3 latch (live-computed in completeTenureApply): a tenure
  /// completed without the recovered witness.
  var servedWithoutRecovery: bool

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
    closeScopeViolated' = closeScopeViolated,
    directCloseApplied' = directCloseApplied,
    foreignSettleApplied' = foreignSettleApplied,
  }
  action claimsUnchanged = all {
    committedClaims' = committedClaims,
    replicaGen' = replicaGen,
  }
  /// Plane-state frames (bughunt-2). Split per plane so an action that
  /// WRITES one plane's state frames the others explicitly.
  action atomicUnchanged = all { atomicGen' = atomicGen }
  action stampLatchUnchanged = all { staleWriteCommitted' = staleWriteCommitted }
  action outboxUnchanged = all {
    latchedExec' = latchedExec,
    latchedStale' = latchedStale,
    staleReplayApplied' = staleReplayApplied,
    successorExecClosed' = successorExecClosed,
  }
  action lifecycleUnchanged = all {
    recoveredOk' = recoveredOk,
    tenureComplete' = tenureComplete,
    servedWithoutRecovery' = servedWithoutRecovery,
  }
  /// Everything the bughunt-2 planes own — the frame the legacy
  /// actions (and the four legacy calibrations' override actions)
  /// carry.
  action planesUnchanged = all {
    atomicUnchanged, stampLatchUnchanged, outboxUnchanged, lifecycleUnchanged,
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
    closeScopeViolated' = false,
    directCloseApplied' = false,
    foreignSettleApplied' = false,
    atomicGen' = REPLICAS.mapBy(r => if (r == "r1") 1 else 0),
    staleWriteCommitted' = false,
    latchedExec' = 0,
    latchedStale' = false,
    staleReplayApplied' = false,
    successorExecClosed' = false,
    recoveredOk' = REPLICAS.mapBy(r => false),
    tenureComplete' = REPLICAS.mapBy(r => false),
    servedWithoutRecovery' = false,
  }

  // --- 8a. oracle-seated apply sub-actions (bughunt-2 bug_358) ----------------
  //
  // The five latches are computed HERE from the same inputs that drive
  // the state change. Live actions and calibration overrides BOTH route
  // through these seats; only the admission/decision differs.

  /// The below-floor oracle: the writer's believed floor exceeds its
  /// generation. Admitted transactions pass their TxFloorRead snapshot;
  /// fenceless calibration writers pass currentFloor.
  def snapshotExceedsGen(r: str, snap: int): bool = snap > replicaGen.get(r)

  /// Row upsert apply — the INSERT/DO UPDATE arm. The EvalPlanQual
  /// re-check (activeGen <= g) is the CALLER's decision; the regression
  /// truth is computed here from the pre-state.
  action upsertApply(r: str, snap: int, g: int): bool = all {
    all {
      activeGen' = g,
      activeOwner' = r,
      activeExec' = execsMinted + 1,
      execsMinted' = execsMinted + 1,
      rowRegressed' = or { rowRegressed, activeGen > 0 and g < activeGen },
    },
    belowFloorMutation' = or { belowFloorMutation, snapshotExceedsGen(r, snap) },
    floorRegressed' = floorRegressed,
    deposedCloseApplied' = deposedCloseApplied,
    clientGaveUp' = clientGaveUp,
    closeScopeViolated' = closeScopeViolated,
    directCloseApplied' = directCloseApplied,
    foreignSettleApplied' = foreignSettleApplied,
  }

  /// Row close apply — clears the active row. Admission and the
  /// named-exec match are the CALLER's decisions; the truths are
  /// computed here from the pre-state (TB-4): `named` is the exec id
  /// the caller's close statement BINDS (production: the
  /// `exec_id = $1` selector; 0 encodes a derivation-keyed close that
  /// names none) — a close that applies while `named` differs from
  /// the row's exec is the scope violation; a live non-owner's
  /// applied close is the failover settle (reachability ghost); a
  /// deposed replica's applied close on a foreign row is the
  /// merged_bug_231 latch, unchanged.
  action closeApply(r: str, snap: int, named: int): bool = all {
    deposedCloseApplied' = or {
      deposedCloseApplied,
      and { activeGen > 0, activeOwner != r, deposed(r) },
    },
    closeScopeViolated' = or {
      closeScopeViolated,
      and { activeGen > 0, named != activeExec },
    },
    directCloseApplied' = or { directCloseApplied, activeGen > 0 },
    foreignSettleApplied' = or {
      foreignSettleApplied,
      and { activeGen > 0, activeOwner != r, not(deposed(r)) },
    },
    activeGen' = 0,
    activeOwner' = "",
    activeExec' = 0,
    execsMinted' = execsMinted,
    belowFloorMutation' = or { belowFloorMutation, snapshotExceedsGen(r, snap) },
    rowRegressed' = rowRegressed,
    floorRegressed' = floorRegressed,
    clientGaveUp' = clientGaveUp,
  }

  /// Floor write apply — writes v VERBATIM. The GREATEST ratchet is the
  /// CALLER's decision; the regression truth is computed here.
  action floorWriteApply(r: str, snap: int, v: int): bool = all {
    floorRegressed' = or { floorRegressed, v < resourceFloor },
    resourceFloor' = v,
    belowFloorMutation' = or { belowFloorMutation, snapshotExceedsGen(r, snap) },
    rowRegressed' = rowRegressed,
    deposedCloseApplied' = deposedCloseApplied,
    clientGaveUp' = clientGaveUp,
    closeScopeViolated' = closeScopeViolated,
    directCloseApplied' = directCloseApplied,
    foreignSettleApplied' = foreignSettleApplied,
  }

  /// Refusal answer apply — the retryable/terminal DECISION is the
  /// caller's; the give-up truth is computed here.
  action answerRefusal(r: str, retryable: bool): bool = all {
    pendingRefusal.get(r),
    pendingRefusal' = pendingRefusal.set(r, false),
    clientGaveUp' = or { clientGaveUp, not(retryable) },
    rowRegressed' = rowRegressed,
    belowFloorMutation' = belowFloorMutation,
    floorRegressed' = floorRegressed,
    deposedCloseApplied' = deposedCloseApplied,
    closeScopeViolated' = closeScopeViolated,
    directCloseApplied' = directCloseApplied,
    foreignSettleApplied' = foreignSettleApplied,
  }

  // --- 8. actions --------------------------------------------------------------------

  /// A replica claims the next generation (the successor). The claim
  /// COMMITS immediately — every later floor READ sees it; an already
  /// in-flight transaction's snapshot does not (READ COMMITTED).
  // Mirrors: rio-scheduler/src/db/recovery.rs:claim_generation
  action successorClaim(r: str): bool = all {
    maxOf(committedClaims) + 1 <= MAX_GEN,
    committedClaims' = committedClaims.union(Set(maxOf(committedClaims) + 1)),
    replicaGen' = replicaGen.set(r, maxOf(committedClaims) + 1),
    // The claim stamp seeds the lease atomic too (atomic >= stamp
    // always; only atomicBumpReacquire moves it further).
    atomicGen' = atomicGen.set(r, maxOf(committedClaims) + 1),
    // A claim opens a NEW tenure: its lifecycle restarts un-recovered
    // and incomplete (plane 3).
    recoveredOk' = recoveredOk.set(r, false),
    tenureComplete' = tenureComplete.set(r, false),
    servedWithoutRecovery' = servedWithoutRecovery,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    rowUnchanged, floorUnchanged, latchesUnchanged,
    stampLatchUnchanged, outboxUnchanged,
  }

  /// begin_fenced's floor read: snapshot the floor VISIBLE NOW.
  ///
  /// OP-5 (bughunt-13): the never-led STANDBY's begin is IN-ALPHABET
  /// — a pooled connection can always open a transaction; the FENCE
  /// is what refuses it (production: a standby's stamp saturates to
  /// 0, below every floor — `ServingGeneration::to_kernel_u64`'s
  /// fail-closed projection; `at_or_above_floor(Some(f), 0)` is
  /// false). The former `replicaGen > 0` guard excluded the exact
  /// trace `sched.lease.standby-drops-writes` quantifies over, so
  /// the rule was green by OMISSION (the enabledness shape). In this
  /// model every reachable floor is >= 1 (init commits claim 1), so
  /// a gen-0 begin can only continue into fenceRefuse — the
  /// nonHolderWriteNeverCommits law below. Out-of-space note: a
  /// FRESH cluster's None-floor admits any generation in production
  /// (`at_or_above_floor(None, _)`) — outside this model's space by
  /// init construction, and the existing fresh-cluster trust posture
  /// (`begin_fenced`'s doc) prices it.
  // Mirrors: rio-scheduler/src/db/mod.rs:begin_fenced
  action beginTx(r: str): bool = all {
    txPhase.get(r) == TxIdle,
    txPhase' = txPhase.set(r, TxFloorRead(currentFloor)),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    planesUnchanged,
  }

  /// begin_fenced refuses: the visible floor exceeds the replica's
  /// generation — Fenced{floor}. The transaction ends having written
  /// NOTHING; the refusal awaits its client answer (bug_393's hook).
  // Mirrors: rio-scheduler/src/db/mod.rs:begin_fenced
  action fenceRefuse(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v > replicaGen.get(r)
    },
    txPhase' = txPhase.set(r, TxIdle),
    pendingRefusal' = pendingRefusal.set(r, true),
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    planesUnchanged,
  }

  /// The GUARDED mint upsert (sched.lease.fence-statement-guard),
  /// parameterized by the generation the write STAMPS — the shared
  /// apply sub-action carrying the plane-1 tenure-stamp oracle
  /// (merged_bug_338, live-computed: any committed write whose stamp
  /// is not the replica's claim-stamped tenure latches). The fixed
  /// system stamps `replicaGen` (the typed ServingGeneration); the
  /// 338 calibration perturbs ONLY this decision (stamps `atomicGen`,
  /// the fresh lease-atomic read).
  ///
  /// The advisory floor compare passed at read time; the in-statement
  /// predicate `assignments.generation <= EXCLUDED.generation`
  /// evaluates against the row's LATEST COMMITTED version
  /// (EvalPlanQual) — NOT the snapshot. Equality passes (same-epoch
  /// re-acquire keep). rows_affected == 0 ⇒ rollback + Fenced.
  action guardedUpsertCommitAs(r: str, writeGen: int): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r) // the advisory half passed
    },
    execsMinted < MAX_EXECS,
    txPhase' = txPhase.set(r, TxIdle),
    val g = writeGen
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(v) => v }
    all {
      // The tenure-stamp oracle: latches on the COMMIT path only.
      staleWriteCommitted' = or {
        staleWriteCommitted,
        activeGen <= g and g != replicaGen.get(r),
      },
      if (activeGen <= g) all {
        // Applies: fresh insert (activeGen == 0 — the priced
        // residual: g may sit below a claims-only floor) or guarded
        // update of an equal-or-older row. Never a regression — the
        // upsertApply oracle re-derives that truth live, against the
        // gen this write actually STAMPS (the plane-1 decision).
        upsertApply(r, snap, g),
        pendingRefusal' = pendingRefusal,
      } else all {
        // Predicate refused against the newer committed row: zero
        // rows, rollback, surfaced as the same retryable refusal.
        rowUnchanged,
        pendingRefusal' = pendingRefusal.set(r, true),
        latchesUnchanged,
      },
    },
    claimsUnchanged, floorUnchanged,
    atomicUnchanged, outboxUnchanged, lifecycleUnchanged,
  }

  /// The fixed system's mint: stamps the claim-stamped tenure.
  // Mirrors: rio-scheduler/src/db/open_attempts.rs:mint_pull_attempt_fenced
  // Mirrors: rio-scheduler/src/db/open_attempts.rs:mint_assignment_upsert_in_tx
  action guardedUpsertCommit(r: str): bool =
    guardedUpsertCommitAs(r, replicaGen.get(r))

  /// Plane 1 environment event: a mid-tenure lease re-acquire/rebound
  /// raises the live atomic ABOVE the claim-stamped tenure (the
  /// mid-mailbox window merged_bug_338 lived in). The claim stamp
  /// does NOT move — only a new claim moves it.
  // Environment: the lease plane's mid-tenure re-acquire/rebound advances the live serving atomic (rio-lease) — no scheduler-DB transition fires; bounded by MAX_GEN, and the claim stamp (the typed ServingGeneration every write carries) does not move
  action atomicBumpReacquire(r: str): bool = all {
    ENABLE_ATOMIC_REREAD,
    atomicGen.get(r) > 0,
    atomicGen.get(r) < MAX_GEN,
    atomicGen' = atomicGen.set(r, atomicGen.get(r) + 1),
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    stampLatchUnchanged, outboxUnchanged, lifecycleUnchanged,
  }

  /// The exec-keyed fenced close (FencedTx::close_assignment), TB-4
  /// re-derived: production's per-attempt closers bind the exec id
  /// their REPORT/SWEEP ROW carries (`exec_id = $1`) and are NOT
  /// owner-gated — assignments rows carry no scheduler-replica owner,
  /// the claims-floor fence is the only gate, and the failover settle
  /// (a live successor closing a predecessor-minted row) is the daily
  /// path. A named exec that is no longer the active one affects ZERO
  /// rows (the resubmit re-mint rewrote exec_id in place — the
  /// merged_bug_011 scope law on the direct path). The named ids
  /// range over MINTED execs only (reports and sweep rows carry real
  /// ids; UUIDs are unguessable).
  ///
  /// Environment bound (TB-4, priced): a DEPOSED replica never
  /// applies a close to a row it does not own — production commit
  /// order (claim ≼ mint ≼ exec-id-visibility) means any read that
  /// could learn a newer tenure's exec id happens after that tenure's
  /// claim is floor-visible, so the deposed replica's begin refuses;
  /// and the sweep's expiry filter cannot select a successor row
  /// minted inside the closer's own open transaction. Encoded as an
  /// enabling condition instead of data-flow provenance state; the
  /// fence-231 calibration drops the whole fence and keeps the
  /// deposed-close latch falsifiable.
  // Mirrors: rio-scheduler/src/db/mod.rs:close_assignment
  action fencedClose(r: str): bool = {
    nondet e = 1.to(MAX_EXECS).oneOf()
    fencedCloseNamed(r, e)
  }

  /// The named-exec close body (runs and calibrations call this
  /// directly with an explicit subject).
  action fencedCloseNamed(r: str, e: int): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r)
    },
    e <= execsMinted,
    not(and { deposed(r), activeGen > 0, activeExec == e, activeOwner != r }),
    txPhase' = txPhase.set(r, TxIdle),
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(v) => v }
    if (activeGen > 0 and activeExec == e) {
      closeApply(r, snap, e)
    } else all {
      rowUnchanged,
      latchesUnchanged,
    },
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    planesUnchanged,
  }

  /// The fenced resource-floor write with the server-side per-
  /// dimension GREATEST ratchet (bug_273's fix): even a live leader's
  /// stale-base write can only raise the floor.
  // Mirrors: rio-scheduler/src/db/derivations.rs:update_resource_floor
  action floorWriteGreatest(r: str, v: int): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(f) => f <= replicaGen.get(r)
    },
    txPhase' = txPhase.set(r, TxIdle),
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(f) => f }
    floorWriteApply(r, snap, if (v > resourceFloor) v else resourceFloor),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged,
    planesUnchanged,
  }

  /// The client answers a fence refusal as RETRYABLE (UNAVAILABLE
  /// family — sched.grpc.fence-retryable): it retries; nobody gives
  /// up. The fixed system's only answer arm.
  // Mirrors: rio-scheduler/src/grpc/actor_guards.rs:actor_error_to_status
  action answerRetryable(r: str): bool = all {
    answerRefusal(r, true),
    txPhase' = txPhase,
    claimsUnchanged, rowUnchanged, floorUnchanged,
    planesUnchanged,
  }

  // --- 8b. bughunt-2 plane actions ------------------------------------------------

  /// Plane 2: a status persist FAILED — the leader latches the batch
  /// with the derivation's ACTIVE exec (memory is the only source).
  /// The active row stays open (the failed persist closed nothing).
  // Mirrors: rio-scheduler/src/actor/mod.rs:latch_status_batch
  action latchFailedPersist(r: str): bool = all {
    ENABLE_OUTBOX,
    latchedExec == 0,
    activeExec > 0,
    activeOwner == r,
    not(deposed(r)),
    latchedExec' = activeExec,
    latchedStale' = false,
    staleReplayApplied' = staleReplayApplied,
    successorExecClosed' = successorExecClosed,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    atomicUnchanged, stampLatchUnchanged, lifecycleUnchanged,
  }

  /// Plane 2: the derivation ADVANCES past the latch — the resubmit
  /// reset re-mints the active row to a FRESH exec (the production
  /// active-row upsert rewrites exec_id in place).
  // Mirrors: rio-scheduler/src/db/open_attempts.rs:mint_assignment_upsert_in_tx
  action advanceDerivation(r: str): bool = all {
    ENABLE_OUTBOX,
    latchedExec > 0,
    not(latchedStale),
    execsMinted < MAX_EXECS,
    activeOwner == r,
    not(deposed(r)),
    activeGen' = replicaGen.get(r),
    activeOwner' = r,
    activeExec' = execsMinted + 1,
    execsMinted' = execsMinted + 1,
    latchedExec' = latchedExec,
    latchedStale' = true,
    staleReplayApplied' = staleReplayApplied,
    successorExecClosed' = successorExecClosed,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged, latchesUnchanged,
    atomicUnchanged, stampLatchUnchanged, lifecycleUnchanged,
  }

  /// Plane 2's shared replay apply — the outbox oracle, live-computed
  /// from the DECISION parameters: `dropStale` (the flush-time
  /// re-derivation: stale entries are DROPPED) and `execScoped` (the
  /// close matches the LATCHED exec only, never the derivation). The
  /// fixed system calls (true, true); the 011 calibration calls
  /// (false, false) — the pre-fix absolute replay.
  action flushReplayApply(r: str, dropStale: bool, execScoped: bool): bool = all {
    ENABLE_OUTBOX,
    latchedExec > 0,
    replicaGen.get(r) > 0,
    not(deposed(r)),
    val applied = not(latchedStale and dropStale)
    val closes = and {
      applied,
      if (execScoped) activeExec > 0 and activeExec == latchedExec
      else activeExec > 0,
    }
    all {
      // The live oracles.
      staleReplayApplied' = or { staleReplayApplied, latchedStale and applied },
      successorExecClosed' = or {
        successorExecClosed,
        closes and activeExec != latchedExec,
      },
      if (closes) all {
        activeGen' = 0,
        activeOwner' = "",
        activeExec' = 0,
        execsMinted' = execsMinted,
      } else {
        rowUnchanged
      },
    },
    latchedExec' = 0,
    latchedStale' = false,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged, latchesUnchanged,
    atomicUnchanged, stampLatchUnchanged, lifecycleUnchanged,
  }

  /// The fixed system's flush: re-derived (stale ⇒ drop) and
  /// exec-scoped.
  // Mirrors: rio-scheduler/src/db/derivations.rs:replay_status_batch_guarded
  action flushReplayGuarded(r: str): bool = flushReplayApply(r, true, true)

  /// Plane 3: this tenure's recovery (the DAG load) succeeds — the
  /// RecoveredDag witness exists.
  // Mirrors: rio-scheduler/src/actor/recovery.rs:recover_from_pg
  action recoverySucceed(r: str): bool = all {
    ENABLE_TENURE_LIFECYCLE,
    replicaGen.get(r) > 0,
    not(recoveredOk.get(r)),
    not(tenureComplete.get(r)),
    recoveredOk' = recoveredOk.set(r, true),
    tenureComplete' = tenureComplete,
    servedWithoutRecovery' = servedWithoutRecovery,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    atomicUnchanged, stampLatchUnchanged, outboxUnchanged,
  }

  /// Plane 3's shared completion apply — the lifecycle oracle,
  /// live-computed from the DECISION parameter `witnessed` (whether
  /// the caller holds the recovered witness). The fixed system's
  /// completeTenure passes `recoveredOk` UNDER its own precondition;
  /// the 155 calibration calls the apply with no precondition (the
  /// retired complete-with-empty-DAG doctrine).
  action completeTenureApply(r: str, witnessed: bool): bool = all {
    ENABLE_TENURE_LIFECYCLE,
    replicaGen.get(r) > 0,
    not(tenureComplete.get(r)),
    servedWithoutRecovery' = or { servedWithoutRecovery, not(witnessed) },
    tenureComplete' = tenureComplete.set(r, true),
    recoveredOk' = recoveredOk,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, rowUnchanged, floorUnchanged, latchesUnchanged,
    atomicUnchanged, stampLatchUnchanged, outboxUnchanged,
  }

  /// The fixed system's completion: only a recovered tenure completes.
  // Mirrors: rio-scheduler/src/actor/recovery.rs:handle_leader_acquired
  action completeTenure(r: str): bool = all {
    recoveredOk.get(r),
    completeTenureApply(r, recoveredOk.get(r)),
  }

  /// Plane 3: a tenure whose recovery has not succeeded steps down
  /// (sched.recovery.step-down): the replica stops serving its
  /// generation — the DURABLE CLAIM STAYS (committedClaims untouched:
  /// the documented harmless over-claim) — and re-enters candidacy.
  // Mirrors: rio-scheduler/src/actor/recovery.rs:request_step_down
  action stepDownFailedTenure(r: str): bool = all {
    ENABLE_TENURE_LIFECYCLE,
    replicaGen.get(r) > 0,
    not(recoveredOk.get(r)),
    not(tenureComplete.get(r)),
    committedClaims' = committedClaims,
    replicaGen' = replicaGen.set(r, 0),
    atomicGen' = atomicGen.set(r, 0),
    recoveredOk' = recoveredOk.set(r, false),
    tenureComplete' = tenureComplete.set(r, false),
    servedWithoutRecovery' = servedWithoutRecovery,
    txPhase' = txPhase,
    pendingRefusal' = pendingRefusal,
    rowUnchanged, floorUnchanged, latchesUnchanged,
    stampLatchUnchanged, outboxUnchanged,
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
      // bughunt-2 planes (gated by their ENABLE_* consts).
      atomicBumpReacquire(r),
      latchFailedPersist(r),
      advanceDerivation(r),
      flushReplayGuarded(r),
      recoverySucceed(r),
      completeTenure(r),
      stepDownFailedTenure(r),
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

  /// OP-5 (bughunt-13): the sched.lease.standby-drops-writes pin —
  /// a NON-HOLDER's write never commits: any transaction whose
  /// begin-time floor exceeded its replica's generation (the
  /// never-led standby at gen 0 — now in-alphabet via the relaxed
  /// beginTx — and the deposed-at-begin ex-leader alike) mutates
  /// nothing. Same oracle seat as belowFloorTxNeverMutates
  /// (snapshotExceedsGen, computed in every mutation apply): the
  /// model's holdership projection IS the generation order, so the
  /// two law names share one latch — this val is the spec rule's
  /// falsifiable verify site, that one is the open-coded-compare
  /// class; each carries its own twin. The in-flight residual (a
  /// transaction begun BEFORE the deposing claim was visible) is
  /// deliberately outside this law and damage-bounded by
  /// activeRowGenMonotonic + directCloseHitsOnlyNamedExec +
  /// openAttemptViewStableUnderDeposedClose + resourceFloorMonotonic.
  /// Falsifiability pair: calibration fence-op5-standby-write.
  val nonHolderWriteNeverCommits = not(belowFloorMutation)

  /// OP-5 non-vacuity witness (expected VIOLATED by its wired
  /// check): the standby's queued write is ATTEMPTED and refused —
  /// the begin+refusal trace is in-space, so
  /// nonHolderWriteNeverCommits is not green by omission (the
  /// enabledness shape this pin retires).
  val noStandbyRefusal = not(REPLICAS.exists(r => and {
    replicaGen.get(r) == 0,
    pendingRefusal.get(r),
  }))

  /// bug_273: the resource floor is monotone.
  /// Falsifiability pair: calibration fence-273-plain-floor-set.
  val resourceFloorMonotonic = not(floorRegressed)

  /// merged_bug_231: a deposed replica never closes an active row it
  /// does not own.
  /// Falsifiability pair: calibration fence-231-unfenced-close.
  val openAttemptViewStableUnderDeposedClose = not(deposedCloseApplied)

  /// TB-4 (bughunt-13): a direct close lands only on the exec it
  /// NAMES — production's `close_assignment` selector (`exec_id =
  /// $1`); a derivation-/owner-keyed close that lands elsewhere is
  /// the merged_bug_231/merged_bug_011 scope class on the
  /// per-attempt path.
  /// Falsifiability pair: calibration
  /// fence-tb4-derivation-keyed-close.
  val directCloseHitsOnlyNamedExec = not(closeScopeViolated)

  /// TB-4 non-vacuity witness (expected VIOLATED by its wired check):
  /// some exec-matched close applies — the re-keyed arm is exercised.
  val noDirectCloseApplied = not(directCloseApplied)

  /// TB-4 non-vacuity witness (expected VIOLATED by its wired check):
  /// the failover settle — a LIVE non-owner closing the row it
  /// names — is inside the verified space (the over-constraint face's
  /// repair proof).
  val noForeignSettle = not(foreignSettleApplied)

  /// bug_393: every fence refusal is answered retryably — no client
  /// ever gives up on a request the live leader would accept.
  /// Falsifiability pair: calibration fence-393-terminal-refusal.
  val fenceRefusalAlwaysRetryable = not(clientGaveUp)

  /// merged_bug_338: every committed write carries its replica's
  /// claim-stamped tenure — never a fresh lease-atomic read.
  /// Falsifiability pair: calibration fence-338-atomic-reread.
  val writesCarryClaimedTenure = not(staleWriteCommitted)

  /// merged_bug_011 (regression half): a latched batch is never
  /// applied after its derivation advanced past the latch.
  /// Falsifiability pair: calibration fence-011-absolute-replay.
  val outboxReplayNeverRegresses = not(staleReplayApplied)

  /// merged_bug_011 (scope half): a replay close only ever lands on
  /// the LATCHED exec — a successor attempt is untouchable.
  /// Falsifiability pair: calibration fence-011-absolute-replay.
  val outboxClosesOnlyLatchedExecs = not(successorExecClosed)

  /// bug_155: no tenure completes without the recovered-DAG witness
  /// (the previously model-unrepresentable zombie made representable).
  /// Falsifiability pair: calibration
  /// fence-155-serve-after-failed-recovery.
  val failedRecoveryNeverServes = not(servedWithoutRecovery)

  /// The wired conjunction.
  val fencedWritesAll = and {
    activeRowGenMonotonic,
    belowFloorTxNeverMutates,
    nonHolderWriteNeverCommits,
    resourceFloorMonotonic,
    openAttemptViewStableUnderDeposedClose,
    directCloseHitsOnlyNamedExec,
    fenceRefusalAlwaysRetryable,
    writesCarryClaimedTenure,
    outboxReplayNeverRegresses,
    outboxClosesOnlyLatchedExecs,
    failedRecoveryNeverServes,
  }
}

/// Tier-1 exhaustive scope: 2 replicas, 3 generations, 1 drv; all
/// three bughunt-2 planes LIVE (this check is the baseline-hold pair
/// of the three plane twins).
module fencedWritesT1 {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    ENABLE_ATOMIC_REREAD = true,
    ENABLE_OUTBOX = true,
    ENABLE_TENURE_LIFECYCLE = true
  ).*

  // --- named conformance runs (OP-2: the mbt-rio-fence replay) -------
  //
  // db/tests/mbt_fence.rs mirrors each action list verbatim and diffs
  // the implementation's projected state against the model's after
  // every step (`quint test --match=^<run>$ --out-itf` is the trace
  // source). The runs double as in-model regression tests: each
  // `.expect` clause re-checks the post-state on every `quint test`.

  /// Mint -> exec-keyed close -> floor ratchet -> successor claim ->
  /// fence refusal -> retryable answer: one pass over every legacy
  /// plane surface the db-level driver maps.
  run fenceHappyPathRun =
    init
      .then(beginTx("r1"))
      .then(guardedUpsertCommit("r1"))
      .then(beginTx("r1"))
      .then(fencedCloseNamed("r1", 1))
      .then(beginTx("r1"))
      .then(floorWriteGreatest("r1", 2))
      .then(successorClaim("r2"))
      .then(beginTx("r1"))
      .then(fenceRefuse("r1"))
      .then(answerRetryable("r1"))
      .expect(and {
        activeGen == 0,
        resourceFloor == 2,
        committedClaims == Set(1, 2),
        not(clientGaveUp),
        not(belowFloorMutation),
      })

  /// The merged_bug_011 acceptance shape (OQ-12): the outbox latches
  /// the active exec, the derivation ADVANCES past the latch (the
  /// resubmit re-mint rewrites exec_id in place), and the guarded
  /// flush applies NOTHING — no stale apply, no close off the latched
  /// exec, the successor attempt survives.
  run fenceOutboxReplayRun =
    init
      .then(beginTx("r1"))
      .then(guardedUpsertCommit("r1"))
      .then(latchFailedPersist("r1"))
      .then(advanceDerivation("r1"))
      .then(flushReplayGuarded("r1"))
      .expect(and {
        activeExec == 2,
        execsMinted == 2,
        latchedExec == 0,
        not(staleReplayApplied),
        not(successorExecClosed),
      })

  /// OP-5: the standby's queued write — a never-led replica's pooled
  /// connection opens a transaction, the fence refuses it at the
  /// floor read, and the refusal is answered retryably. The rule's
  /// own trace (sched.lease.standby-drops-writes), previously
  /// outside the alphabet.
  run fenceStandbyRefusalRun =
    init
      .then(beginTx("r2"))
      .then(fenceRefuse("r2"))
      .then(answerRetryable("r2"))
      .expect(and {
        replicaGen.get("r2") == 0,
        not(belowFloorMutation),
        not(clientGaveUp),
        activeGen == 0,
        execsMinted == 0,
      })

  /// The failover settle (TB-4 face (b) coverage): a LIVE successor
  /// closes the exec a predecessor minted — the daily report/sweep
  /// path the former owner gate excluded from the verified space.
  run fenceForeignSettleRun =
    init
      .then(beginTx("r1"))
      .then(guardedUpsertCommit("r1"))
      .then(successorClaim("r2"))
      .then(beginTx("r2"))
      .then(fencedCloseNamed("r2", 1))
      .expect(and {
        activeGen == 0,
        foreignSettleApplied,
        not(deposedCloseApplied),
        not(closeScopeViolated),
      })
}
