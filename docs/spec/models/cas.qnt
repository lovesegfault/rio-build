// -*- mode: Bluespec; -*-
//
// The CAS fragment of the leader-election protocol: the rv-guarded apiserver
// PUT and the at-most-one-CAS-winner invariant. This is the smallest piece of
// the leader-election model with a real invariant, a real race, and a
// known-correct reference to validate against — it is cross-validated against
// the full model's `casOk` / `atMostOneCASWinner` in `leaderElection.qnt`
// (the same invariant, the same counterexample when the guard is deleted).
//
// THIS SPEC IS ALSO THE PERMANENT QUINT-TOOLCHAIN SMOKE CHECK
// (`checks.quint-cas-smoke`): the same role `cov-smoke` plays for the coverage
// pipeline. It is small enough to verify in ~1s, so "the quint toolchain is
// broken" (a backend regression, a flake-input bump that loses the bundled
// Apalache dist, a sandbox PATH change) is caught at merge-gate rather than at
// the next model change. Do not delete it when the full leader-election port
// lands — redundancy with the full model is the point.
//
// Correspondence with the full model (`leaderElection.qnt`):
//
//   lease            ~ the full model's `lease` record (holder, rv; the `gen`
//                      field is the generation arc, stripped here)
//   apiGet(n)        ~ apiGet(n): snapshot the lease's resourceVersion
//   apiPut(n)/tryPut ~ steal(n) conjoined with casOk(n): the rv-guarded PUT.
//                      The staleness precondition, clocks, generations, and
//                      self-fence are stripped — any node with a snapshot may
//                      attempt a PUT.
//   acquiredAt, casRace, atMostOneCASWinner ~ verbatim.
//
// State-space bound (MAX_RV): Apalache bounds by STEP COUNT, so it does not
// need one — but the TLC backend (`quint verify --backend=tlc`) enumerates the
// reachable state space exhaustively and an unbounded rv makes that space
// infinite (measured: depth 763, 36.8M distinct states, still growing, before
// the ceiling was added). Same discipline as every model in this
// directory: the ceiling is an action PRECONDITION (the PUT is rejected at
// MAX_RV), not a saturating clamp — saturation would let two nodes acquire at
// the same ceiling value and falsify the invariant as a state-space artifact.
//
// VALIDATION (cross-checked against the full model at migration time;
// re-verify after any edit):
//   - atMostOneCASWinner HOLDS over the full state space (the state count,
//     depth, and wall-clock are in the check's output transcript and the
//     introducing commit — never here).
//   - THE WEAKENED TEST (manual procedure — run it whenever the guard logic
//     changes): delete the `s.lease.rv == mySnap` conjunct from `tryPut`'s
//     guard and re-run `quint verify --backend=tlc --invariant=atMostOneCASWinner`.
//     The invariant MUST be violated with a shallow trace (both nodes GET the
//     empty lease at rv 0, n1 PUTs and wins, n2's stale PUT also wins,
//     casRace flips). If the weakened spec stops producing this counterexample
//     the invariant has gone vacuous. Restore the conjunct afterwards.
//   - The contended state (a stale snapshot held while another node leads at
//     that snapshot's rv) is REACHABLE: the `casContentionReachable` witness
//     is violated. If it stops being violated, the guard is never load-bearing
//     and atMostOneCASWinner proves nothing.
module cas {
  // --- 1. types ---------------------------------------------------------
  type Node = str
  type Holder = | NoHolder | Held(Node)
  type Lease = { holder: Holder, rv: int }
  type State = {
    lease: Lease,
    // Per-node snapshot of the lease's rv from the node's last GET.
    // NO_RV means "no snapshot in hand" (the full model's snap[n] with
    // no holder/rv recorded).
    snapRv: Node -> int,
    leading: Node -> bool,
    // The rv at which the node's PUT succeeded. NO_RV = never acquired
    // (the full model's acquiredAt[n] sentinel).
    acquiredAt: Node -> int,
    // History flag: a PUT succeeded at an rv another live leader already
    // acquired at — the CAS admitted two writers. Must never become true.
    casRace: bool,
  }

  // --- 2. constants -----------------------------------------------------
  pure val NODES = Set("n1", "n2")
  pure val NO_RV = -1
  // The state-space ceiling for the TLC backend (see the header). 5 admits
  // every interleaving class the invariant is about: both nodes can acquire,
  // be deposed, and re-acquire at distinct rvs before the ceiling disables
  // further PUTs.
  pure val MAX_RV = 5

  // --- 3. pure logic ----------------------------------------------------
  /// The rv-guarded PUT (the apiserver's optimistic-concurrency replace()).
  /// THE CAS PRECONDITION is `s.lease.rv == mySnap`: the object has not
  /// changed since this node's GET. The weakened test documented in the
  /// header deletes exactly that conjunct.
  pure def tryPut(s: State, n: Node): { success: bool, newState: State } = {
    val mySnap = s.snapRv.get(n)
    val guardOk = and {
      mySnap != NO_RV,        // must hold a snapshot (the full model's hasSnap)
      s.lease.rv == mySnap,   // THE CAS: 409 if the rv moved since the GET
      s.lease.rv < MAX_RV,    // state-space ceiling (TLC backend); see the header
    }
    if (guardOk) {
      // Did the CAS just admit a second writer at an rv someone already
      // acquired at? Evaluated against the PRE-state, like ReplaceGuard.
      val raced = NODES.exists(m => and {
        m != n,
        s.leading.get(m),
        s.acquiredAt.get(m) == mySnap,
      })
      {
        success: true,
        newState: {
          ...s,
          lease: { holder: Held(n), rv: mySnap + 1 },
          leading: s.leading.put(n, true),
          acquiredAt: s.acquiredAt.put(n, mySnap),
          snapRv: s.snapRv.put(n, NO_RV),   // the snapshot is spent
          casRace: s.casRace or raced,
        },
      }
    } else {
      // The apiserver answers 409: the PUT is rejected, nothing changes.
      { success: false, newState: s }
    }
  }

  // --- 4. state variables -----------------------------------------------
  var lease: Lease
  var snapRv: Node -> int
  var leading: Node -> bool
  var acquiredAt: Node -> int
  var casRace: bool

  val currentState: State =
    { lease: lease, snapRv: snapRv, leading: leading, acquiredAt: acquiredAt, casRace: casRace }

  // --- 5. invariants and witnesses --------------------------------------
  /// SAFETY (must be SATISFIED): the apiserver's optimistic concurrency
  /// admits at most one writer per resourceVersion.
  val atMostOneCASWinner = not(casRace)

  /// WITNESS (must be VIOLATED): the contended state — some node holds a
  /// stale snapshot at an rv another node has already acquired at — is
  /// reachable. If it is not, the rv guard is never load-bearing and
  /// atMostOneCASWinner is vacuously true.
  val casContentionReachable = not(
    NODES.exists(n => NODES.exists(m => and {
      m != n,
      snapRv.get(n) != NO_RV,
      leading.get(m),
      acquiredAt.get(m) == snapRv.get(n),
    }))
  )

  /// WITNESS (must be VIOLATED): a PUT can succeed at all.
  val someoneCanLead = not(NODES.exists(n => leading.get(n)))

  /// SAFETY (must be SATISFIED): a second, trivially-true invariant whose only
  /// job is to keep the multi-invariant code path permanently exercised by the
  /// smoke check: `--invariant=a,b` conjoins all invariants into a single
  /// generated `q_inv`, so a violation does NOT name the failing conjunct —
  /// the smoke check pins that the conjunction path itself keeps working.
  val rvInBounds = and { lease.rv >= 0, lease.rv <= MAX_RV }

  // --- 6. actions -------------------------------------------------------
  action unchangedAll = all {
    lease' = lease, snapRv' = snapRv, leading' = leading,
    acquiredAt' = acquiredAt, casRace' = casRace,
  }

  /// GET: snapshot the lease's current rv. Always succeeds.
  action apiGet(n: Node): bool = all {
    snapRv' = snapRv.put(n, lease.rv),
    lease' = lease, leading' = leading, acquiredAt' = acquiredAt, casRace' = casRace,
  }

  /// PUT: thin wrapper over tryPut. A rejected PUT (stale rv) is a no-op
  /// rather than a disabled action so deterministic runs can assert on
  /// the rejection.
  action apiPut(n: Node): bool = {
    val result = tryPut(currentState, n)
    if (result.success) all {
      lease' = result.newState.lease,
      snapRv' = result.newState.snapRv,
      leading' = result.newState.leading,
      acquiredAt' = result.newState.acquiredAt,
      casRace' = result.newState.casRace,
    } else unchangedAll
  }

  // --- 7. init ----------------------------------------------------------
  action init = all {
    lease' = { holder: NoHolder, rv: 0 },
    snapRv' = NODES.mapBy(n => NO_RV),
    leading' = NODES.mapBy(n => false),
    acquiredAt' = NODES.mapBy(n => NO_RV),
    casRace' = false,
  }

  // --- 8. step ----------------------------------------------------------
  action step = {
    nondet n = NODES.oneOf()
    any { apiGet(n), apiPut(n) }
  }

  // --- 9. deterministic run ---------------------------------------------
  /// The executable version of the CAS-race trace documented in prose in
  /// the full model's header: both nodes GET the empty lease at rv 0, n1
  /// PUTs and wins, n2's PUT with its now-stale rv-0 snapshot is rejected.
  run casRaceScenario = {
    init
      .then(apiGet("n1"))
      .then(apiGet("n2"))
      .expect(and { snapRv.get("n1") == 0, snapRv.get("n2") == 0 })
      .then(apiPut("n1"))
      .expect(and {
        leading.get("n1"),
        lease.rv == 1,
        acquiredAt.get("n1") == 0,
        snapRv.get("n2") == 0,        // n2 still holds the stale snapshot
      })
      .then(apiPut("n2"))                // the stale PUT...
      .expect(and {
        not(leading.get("n2")),       // ...is rejected (lease.rv is 1, snap is 0)
        lease.rv == 1,                // ...and changed nothing
        not(casRace),                 // the CAS admitted exactly one writer
      })
  }
}
