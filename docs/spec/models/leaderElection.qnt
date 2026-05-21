// -*- mode: Bluespec; -*-
//
// Phase-1 model of rio-lease's apiserver-CAS leader election protocol:
// per-node monotonic clocks with bounded skew, the observed-record
// staleness clock, local self-fencing, the PG generation high-water
// mark, pod crash/recovery, and Lease-object deletion.
//
// This is the Quint port of (and the replacement for) the hand-written
// TLA+ model formerly at docs/spec/models/LeaderElection.tla. The port
// was validated against every documented result of that predecessor
// before it was removed: each fault regime's distinct-state count is
// exactly |NODES|! times the symmetry-reduced original's (minus the
// self-symmetric states), every weakened test reproduces its
// counterexample, and every non-vacuity probe still fires — the
// migration commits carry the figures. The action-by-action
// correspondence with the TLA+ source is deliberate; the "TLA+ X"
// annotations throughout map this file's encoding onto the
// predecessor's for anyone reading the pre-migration history.
//
// Formalizes (from docs/spec/components/scheduler.typ; update both when
// the protocol changes -- `tracey bump` on a rule flags the r[verify]
// marker in nix/quint.nix as stale):
//   - sched.lease.at-most-one-leader  (the safety invariant, both halves)
//   - sched.lease.k8s-lease           (the protocol mechanism)
//   - sched.recovery.fetch-max-seed   (the generation seeding on acquire)
//   - sched.lease.generation-claim    (the write-ahead claim in steal;
//     verified by the Deletion regime, which injects the fault the claim
//     exists to survive)
//
// CONFORMANCE: beyond the per-regime model checks wired in
// nix/quint.nix, the mbt-rio-lease check in the same file replays
// traces generated from leaderElectionBase against the real rio-lease
// election machinery (rio-lease/src/mbt_tests.rs) and diffs the
// projected state after every step -- drift between this model and the
// implementation surfaces as a red check, not a review-time judgment
// call.
//
// PROOF CLAIM, stated honestly: the invariants below hold under clock
// skew <= MAX_SKEW ticks, no host suspend, over a bounded state space of
// 2 nodes and ~2 leadership cycles, with at most one fault per class per
// trace (leaderElectionBase pins all fault budgets to 0; the Deletion /
// Asymmetric / PgFaults regimes each enable one fault class or constant
// regime -- same core module, a different five-line instantiation). The
// bounds exclude: 3-party races (any k-party CAS race contains a 2-party
// prefix, so atMostOneCASWinner is unaffected; a 3-party dual-belief
// scenario needs two simultaneously-stale ex-leaders and is only
// partially covered), a third leadership handoff, and a second fault of
// any class. The PG-side faults (skipped claim writes, point-in-time
// restores) are each survivable alone -- the Lease's transition count is
// an independent epoch source -- and only collide in conjunction with a
// deletion. See each regime module's header for its coverage-vs-cost
// argument and non-vacuity evidence.
//
// INVARIANTS, and which half of sched.lease.at-most-one-leader+3 each
// verifies:
//
// atMostOneCASWinner -- the hard half: the apiserver's optimistic
// concurrency admits at most one writer per resourceVersion. Weakened
// test: deleting the CAS precondition from casOk reproduces the
// kube-leader-election 0.43 bug (both racers GET rv=0, both PUT, both
// Leading -- and both reach gen=2, so the generation fence cannot tell
// them apart either).
//
// boundedDualLeadership -- the soft half: two replicas CAN concurrently
// believe they lead (a deposed leader that has not yet noticed), but
// every reachable dual-belief state already has a discovery mechanism
// armed -- the stale believer is within 2*MAX_SKEW of its own self-fence
// deadline, or its snapshot is stale (next renew 409s), or its snapshot
// already names the thief, or it was deposed by a Lease deletion (then
// the bound degrades to its own self-fence deadline, FENCE_AFTER + RENEW,
// since no thief ever measured its staleness). Combined with loopInterval
// (the loop runs every RENEW ticks), the window is bounded to one loop
// iteration plus the skew penalty (or one lease lifetime for a deletion
// victim). Weakened test: relaxing steal's staleness threshold from
// > STEAL_AFTER to >= 0 violates it -- the > STEAL_AFTER threshold is
// exactly what guarantees the victim is already at (or within
// 2*MAX_SKEW - separation of) its own fence deadline when deposed.
// Clock skew eats directly into that guarantee and the fence/steal
// separation buys it back: at separation >= RENEW + 2*MAX_SKEW the
// victim has ALREADY fenced (or its clock has stopped at the
// loopInterval cap) when the thief is first allowed to steal, and
// neverDual holds instead. See the TWO OPERATING REGIMES note below.
//
// staleLeaderHasStaleGeneration -- the bridge to
// sched.lease.generation-fence+2: when the dual-belief window is open,
// the executor must be able to tell the replicas apart by generation
// (the holder's strictly greatest when a holder exists among the
// believers; merely distinct when a deletion has deposed both). HOLDS
// over the full state space of every regime now that the generation
// derives from the lease's transition count and is claimed in PG at
// acquisition time. The pre-fix protocol (in-memory fetch_add seeded
// from a lazily-written PG high-water mark) falsified it both with and
// without a deletion; the weakened test (move the genHW update back to
// a dispatch-time persist) still reproduces both counterexamples. See
// the FIXED block at the invariant definition.
//
// loopInterval -- the production loop's tick discipline as a tripwire
// invariant; see its definition.
//
// neverDual -- the healthy-regime invariant (no dual belief at all);
// checked for real only in the Asymmetric regime, deliberately VIOLATED
// in the base regime where it doubles as the non-vacuity probe for the
// dual-belief antecedent.
//
// boundsOK -- the runtime residual of the TLA+ TypeOK: the integer range
// constraints the ceilings are supposed to enforce. The rest of TypeOK
// (map domains, booleans, the nullable records) is subsumed by Quint's
// static type system. A ceiling precondition that is dropped shows up
// here.
//
// ACTION <-> CODE CORRESPONDENCE (rio-lease/src/{lib,election}.rs):
//   tick        clock advance; CLOCK_MONOTONIC ticks regardless of state
//   apiGet      try_acquire_or_renew()'s GET + decide()'s ObservedUpdate
//   steal       decide()->Steal + replace(steal) + on_acquire(trans) +
//               seed_generation_from() + claim_generation() collapsed
//               into one atomic step (sound: recovery_complete gates
//               dispatch until the seed AND the claim have both run)
//   renewLease  decide()->Renew + replace(renew) + the acquire arm's
//               on_acquire(trans) with the UN-bumped count: restores
//               Leading after a self-fence false alarm or a crash at the
//               retained (or crash-restored) generation -- the same-epoch
//               re-acquire and the idempotent self re-claim
//   conflict    the 409 path; collapses 409-on-renew (lose) and
//               409-on-steal (never led) -- both reset the fence clock
//               and end not-leading
//   selfFence   maybe_self_fence(); may fire any time past the deadline
//               (a superset of the production every-RENEW_INTERVAL
//               schedule -- sound for safety)
//   crash/      pod restart: in-memory state lost, gen resets to the
//   recover     AtomicU64::new(1) init; OS clock, apiserver, PG persist
//   deleteLease `kubectl delete lease` + the next replica's create():
//               holder and leaseTransitions reset, resourceVersion does
//               not, PG survives. Disabled (MAX_DELETES=0) in the base
//               regime.
//   (claimOk/claimFails are the named claim disjunction inside
//               steal/renewLease, not step-level actions: the claim
//               INSERT lands (claimOk) or errors with recovery
//               proceeding unclaimed (claimFails -- the documented
//               proceed-on-failure path). claimFails is disabled
//               (MAX_CLAIM_FAILURES=0) everywhere except PgFaults.)
//   pgRestore   a PostgreSQL point-in-time restore regresses the floor
//               to 0; the Lease object survives. Disabled
//               (MAX_RESTORES=0) everywhere except PgFaults.
//
// ENCODING DECISIONS (vs the TLA+ predecessor; each changes the state
// count, none changes which invariants hold):
//   - leading: Node -> bool replaces state \in [Nodes -> {"Following",
//     "Leading"}]. leading.get(n) <=> state[n] = "Leading".
//   - The NULL model value becomes sum types (Holder, SnapState,
//     ObsState) plus a -1 sentinel for acquiredAt (NO_ACQ; a -1 can
//     never equal a real rv >= 0, mirroring NULL /= <int>).
//   - ClockSkewBound was a state CONSTRAINT in the TLA+; the TLC backend
//     cannot express CONSTRAINT, so it is a tick precondition here. The
//     difference: TLC checked invariants on the just-past-the-boundary
//     states before pruning them (states outside the proof claim); the
//     precondition never generates them. Strictly tighter to the stated
//     proof claim ("safe under skew <= MAX_SKEW"); strictly fewer states.
//   - No symmetry reduction (SYMMETRY perm is not expressible under the
//     TLC backend). Expect ~2x the TLA+ state counts for a 2-node
//     symmetric model.
//   - Actions are GUARDED (disabled when their preconditions fail), not
//     thin always-enabled no-ops -- the closest match to the TLA+
//     transition structure. The MBT-oriented pure-def extraction
//     (tryX(state, n) -> {success, newState}) is deferred to the MBT
//     adoption phase; restructuring then is mechanical.
//   - snap/obs reads go through accessor defs (snapRvOf etc.) that
//     return -1 for the NoSnap/NoObs case; every guard still checks
//     hasSnap/hasObs first (the TLA+ snap[n] /= NULL conjunct), and the
//     -1 default fails every comparison that matters if a guard is ever
//     missed.
//
// TWO OPERATING REGIMES (the asymmetric-TTL split): FENCE_AFTER and
// STEAL_AFTER are separate constants. A Leading victim's measured
// staleness on a thief's clock is at most FENCE_AFTER + RENEW + 2*MAX_SKEW
// (the loopInterval tick cap plus the round-trip skew), so:
// When STEAL_AFTER - FENCE_AFTER < RENEW + 2*MAX_SKEW (the base and
// Deletion regimes: zero separation), a thief can clear its steal
// threshold while the victim still believes -- dual belief is reachable
// and boundedDualLeadership is the operative property. When STEAL_AFTER -
// FENCE_AFTER >= RENEW + 2*MAX_SKEW (the Asymmetric regime, separation
// 3 = 1 + 2 with equality), the steal threshold is unreachable while the
// victim still believes and neverDual holds: no dual-belief state exists
// at all. Both boundaries are measured: the separation-2 run violates
// neverDual, the separation-3 run proves it.
//
// PHASE-2 DEFERRALS: liveness (eventually-some-leader needs an unbounded
// clock; the temporal form stutters one tick short of the fence deadline
// forever); NoDualDispatch (the worker-side model that takes
// staleLeaderHasStaleGeneration as an assumption); a second deletion per
// trace; model-based testing of the Deletion/PgFaults regimes (the
// mbt-rio-lease check drives leaderElectionBase only -- the fault
// regimes need the claims-ledger mock and genHW in the projection).
module leaderElection {
  // --- 1. types ---------------------------------------------------------
  type Node = str
  /// The Lease object's holderIdentity: empty or a node.
  type Holder = | NoHolder | Held(Node)
  /// The Lease object as the apiserver stores it: holder, resourceVersion,
  /// leaseTransitions (the `gen` field -- the transition count, NOT a
  /// node's in-memory generation).
  type LeaseRec = { holder: Holder, rv: int, gen: int }
  /// A node's last GET'd copy of the lease (TLA+ snap[n], NULL when the
  /// node has no snapshot in hand / the snapshot was spent).
  type SnapState = | NoSnap | Snap(LeaseRec)
  /// A node's observed-record staleness clock (TLA+ obs[n]): the rv it is
  /// watching and the local-clock instant it started watching.
  type ObsState = | NoObs | Observed({ rv: int, since: int })

  // --- 2. constants (const = overridable per fault regime) --------------
  const NODES: Set[Node]
  /// SELF_FENCE_AFTER in ticks: the leader self-fences after this long
  /// without a successful round-trip. "The leader deciding it no longer
  /// leads."
  const FENCE_AFTER: int
  /// STEAL_AFTER in ticks: a follower steals after observing the same rv
  /// for this long. "The follower deciding the leader is dead." The two
  /// were a single Ttl = LEASE_TTL before the asymmetric-TTL split;
  /// STEAL_AFTER - FENCE_AFTER is the safety margin that decides which
  /// regime an instantiation models (see neverDual).
  const STEAL_AFTER: int
  /// RENEW_INTERVAL in ticks (FENCE_AFTER/RENEW ~= 2-3).
  const RENEW: int
  /// Clock skew bound (the proof assumption).
  const MAX_SKEW: int
  /// Clock ceiling (state-space bound).
  const MAX_TIME: int
  /// Generation ceiling (state-space bound).
  const MAX_GEN: int
  /// resourceVersion ceiling (state-space bound).
  const MAX_RV: int
  /// Lease-object deletions per trace (0 = fault disabled).
  const MAX_DELETES: int
  /// Failed claim INSERTs per trace (0 = disabled). The production
  /// proceed-on-failure path: PG dies between the seed read and the claim
  /// write, recovery logs + counts + proceeds, the leader dispatches at a
  /// generation the ledger does not contain.
  const MAX_CLAIM_FAILURES: int
  /// PG point-in-time restores per trace (0 = disabled). Regresses the
  /// claims ledger AND the assignment history together (the floor is
  /// GREATEST over both).
  const MAX_RESTORES: int

  /// acquiredAt sentinel: "never acquired" (the TLA+ acquiredAt[n] =
  /// NULL). -1 can never equal a real rv (>= 0), mirroring a model
  /// value's inequality with every integer.
  pure val NO_ACQ = -1

  // --- 3. state variables ----------------------------------------------
  /// Per-node monotonic clock, 0..MAX_TIME.
  var clocks: Node -> int
  /// The Lease object at the apiserver.
  var lease: LeaseRec
  /// Process running? (false = crashed, awaiting recover).
  var alive: Node -> bool
  /// Belief: does this node think it is the leader? (TLA+ state[n] =
  /// "Leading"). is_leader in production.
  var leading: Node -> bool
  /// Last GET'd lease (spent on a successful PUT).
  var snap: Node -> SnapState
  /// Observed-record staleness clock.
  var obs: Node -> ObsState
  /// Local-clock instant of the last successful apiserver round-trip
  /// (last_successful_renew). The self-fence deadline is anchored here.
  var fence: Node -> int
  /// In-memory generation (the Arc<AtomicU64>). Lost on crash.
  var gen: Node -> int
  /// PG generation high-water mark: GREATEST(assignments, claims).
  /// Persistent across crashes; regressed only by pgRestore.
  var genHW: int
  /// The rv at which this node's last ACQUISITION (steal) succeeded.
  /// What the CAS race detector compares against. NO_ACQ = never.
  var acquiredAt: Node -> int
  /// History flag: a PUT succeeded at an rv another live leader already
  /// acquired at -- the apiserver let two writers through.
  var casRace: bool
  /// Lease-object deletions so far (0..MAX_DELETES).
  var deletes: int
  /// Holders deposed by a deletion rather than a steal -- their
  /// dual-belief window is bounded by their own self-fence deadline, not
  /// the thief's staleness measurement (see boundedDualLeadership's
  /// fourth disjunct).
  var delVictims: Set[Node]
  /// Failed claim INSERTs so far (0..MAX_CLAIM_FAILURES).
  var claimFailures: int
  /// PG point-in-time restores so far (0..MAX_RESTORES).
  var restores: int

  // --- 4. accessor / guard helpers --------------------------------------
  // The TLA+ reads snap[n].rv directly after guarding snap[n] /= NULL;
  // Quint's sum types force the NoSnap case to be handled. The accessors
  // return -1 for the absent case -- a value that fails every comparison
  // a guard cares about (rv >= 0, holder = Held(...)) -- and every
  // action still checks hasSnap/hasObs first, exactly where the TLA+
  // checks /= NULL.
  def hasSnap(n: Node): bool = snap.get(n) != NoSnap
  def snapRvOf(n: Node): int = match snap.get(n) { | NoSnap => -1 | Snap(l) => l.rv }
  def snapHolderOf(n: Node): Holder = match snap.get(n) { | NoSnap => NoHolder | Snap(l) => l.holder }
  def hasObs(n: Node): bool = obs.get(n) != NoObs
  def obsRvOf(n: Node): int = match obs.get(n) { | NoObs => -1 | Observed(o) => o.rv }
  def obsSinceOf(n: Node): int = match obs.get(n) { | NoObs => -1 | Observed(o) => o.since }

  /// The shared CAS guard for steal/renewLease (TLA+ ReplaceGuard's
  /// precondition half). NOT a standalone action -- a predicate both
  /// PUT actions conjoin.
  def casOk(n: Node): bool = and {
    hasSnap(n),                // snap[n] /= NULL
    snapRvOf(n) < MAX_RV,      // state-space bound on the rv
    // THE CAS PRECONDITION. The deliberately-weakened CAS test
    // documented in the header deletes the next line.
    lease.rv == snapRvOf(n),
  }

  /// ReplaceGuard's casRace history update: flips when this node's PUT
  /// succeeds at an rv that another currently-Leading node also acquired
  /// at -- the apiserver let two writers through. Evaluated against the
  /// PRE-state (the other nodes' acquiredAt before this PUT lands).
  def casRaceAfterPut(n: Node): bool = casRace or NODES.exists(m => and {
    m != n,
    alive.get(m),
    leading.get(m),
    acquiredAt.get(m) == snapRvOf(n),
  })

  // --- 5. invariants and witnesses --------------------------------------

  /// The proof assumption: clocks are bounded-skew. A state CONSTRAINT in
  /// the TLA+ original; enforced by tick's precondition here (the TLC
  /// backend cannot express CONSTRAINT). Listed as an invariant purely as
  /// a tripwire on the precondition.
  val clockSkewBound = NODES.forall(m => NODES.forall(n =>
    clocks.get(m) - clocks.get(n) <= MAX_SKEW))

  /// The runtime residual of the TLA+ TypeOK: the integer ranges the
  /// ceiling preconditions are supposed to enforce. Everything else
  /// (map domains, booleans, the nullable records) is subsumed by the
  /// static type system.
  val boundsOK = and {
    NODES.forall(n => clocks.get(n) >= 0 and clocks.get(n) <= MAX_TIME),
    NODES.forall(n => fence.get(n) >= 0 and fence.get(n) <= MAX_TIME),
    NODES.forall(n => gen.get(n) >= 1 and gen.get(n) <= MAX_GEN),
    NODES.forall(n => acquiredAt.get(n) == NO_ACQ or
      (acquiredAt.get(n) >= 0 and acquiredAt.get(n) <= MAX_RV)),
    lease.rv >= 0, lease.rv <= MAX_RV,
    lease.gen >= 0, lease.gen <= MAX_GEN,
    genHW >= 0, genHW <= MAX_GEN,
    deletes >= 0, deletes <= MAX_DELETES,
    claimFailures >= 0, claimFailures <= MAX_CLAIM_FAILURES,
    restores >= 0, restores <= MAX_RESTORES,
    delVictims.subseteq(NODES),
  }

  /// THE hard half of sched.lease.at-most-one-leader+3: the apiserver's
  /// optimistic concurrency admits at most one writer per
  /// resourceVersion. casRace is flipped by casRaceAfterPut(n) when a PUT
  /// succeeds at an rv that another currently-Leading node also acquired
  /// at -- i.e. the CAS let two writers through. With the precondition
  /// (lease.rv == snapRvOf(n)) this is unreachable: the second writer's
  /// snapshot is stale, its PUT 409s. Without it (delete the
  /// precondition from casOk), both racers GET rv=R, both PUT, both
  /// believe they won -- the kube-leader-election 0.43 bug.
  ///
  /// Deliberately-weakened test (run once during development, NOT in
  /// CI): deleting `lease.rv == snapRvOf(n)` from casOk produces a
  /// counterexample: both nodes GET the empty lease at rv=0, n1 steals
  /// (Leading, acquiredAt=0), n2 steals at its stale rv=0 snapshot --
  /// the PUT succeeds without the precondition, both Leading, casRace
  /// flips. Both also reach gen=2: the generation fence cannot tell them
  /// apart either.
  val atMostOneCASWinner = not(casRace)

  /// Two nodes simultaneously believing they lead. The antecedent of
  /// boundedDualLeadership and staleLeaderHasStaleGeneration; reachable
  /// in the zero-separation regimes, unreachable in the asymmetric one.
  val dual = NODES.exists(m => NODES.exists(n =>
    m != n and leading.get(m) and leading.get(n)))

  /// The healthy-regime invariant: no two replicas ever simultaneously
  /// believe they lead. Holds iff the fence/steal separation covers both
  /// the loopInterval slack and the round-trip clock skew:
  ///   STEAL_AFTER - FENCE_AFTER >= RENEW + 2*MAX_SKEW.
  /// A thief's measured staleness is anchored to the victim's last
  /// completed write (the rv change); the victim's fence is anchored to
  /// the same event (a bare read does not move it -- see apiGet); the
  /// victim's clock cannot advance more than FENCE_AFTER + RENEW past
  /// that anchor while it still believes (the loopInterval tick cap); the
  /// thief's clock is within MAX_SKEW of the victim's and its observation
  /// anchor is within MAX_SKEW the other way. So the thief's measured
  /// staleness while the victim still believes is at most
  /// FENCE_AFTER + RENEW + 2*MAX_SKEW, and a steal threshold at or above
  /// that is unreachable until the victim has stopped believing. Checked
  /// in the Asymmetric regime (separation 3 >= 1 + 2, holds; separation
  /// 2, violated -- the boundary is measured from both sides).
  /// Deliberately VIOLATED in the base and Deletion regimes (zero
  /// separation), where it doubles as the non-vacuity probe for
  /// boundedDualLeadership: dual belief is reachable there, and every
  /// instance has a discovery mechanism armed. That violation is
  /// machine-checked: the expect-violation witness check
  /// quint-leader-election-witness-dual-belief (nix/quint.nix) fails if
  /// dual belief ever stops being reachable in the base regime.
  val neverDual = not(dual)

  // --- expect-violation witnesses (non-vacuity probes) -------------------
  //
  // The vals below are NOT invariants and must never be added to a
  // regime check's invariant list. Each states "the interesting thing
  // never happens"; the quint-leader-election-witness-* checks in
  // nix/quint.nix verify one each and PASS only when the checker reports
  // a violation -- machine-checked evidence that the scenario a regime's
  // headline invariant constrains is actually reachable in that regime's
  // explored space. (neverDual above doubles as the base regime's
  // witness.)

  /// Violated when a second acquisition (a steal of a previously-held
  /// lease) occurs: the transition count reaches 2 only via a steal of a
  /// lease already acquired once -- renewal never bumps it. Checked
  /// against the asymmetric regime, where its violation proves the
  /// contention neverDual constrains still arises under the widened
  /// steal threshold; a constant change that made steals unreachable
  /// within the clock ceiling would leave neverDual vacuously true and
  /// turn this witness's check red.
  val atMostOneAcquisition = lease.gen < 2

  /// Violated when the Lease object has been deleted and subsequently
  /// re-acquired: deletion resets the transition count to zero, so a
  /// positive count alongside a positive deletion counter can only mean
  /// a post-deletion steal. Checked against the deletion regime, where
  /// its violation proves the steal-delete-steal sequence the write-ahead
  /// claim defends against is actually explored -- that regime's
  /// staleLeaderHasStaleGeneration verdict is about a state space
  /// containing the fault sequence, not one where the fault never bites.
  val noReacquisitionAfterDeletion = not(deletes > 0 and lease.gen > 0)

  /// The production loop's tick discipline, stated as a predicate over
  /// states: a Leading node is never more than FENCE_AFTER + RENEW past
  /// its last successful round-trip, because run_lease_loop() checks
  /// maybe_self_fence() every RENEW_INTERVAL and cannot skip the check.
  /// This is ENFORCED by tick's precondition (see the comment there for
  /// why a precondition and not a state constraint); it is listed as an
  /// invariant purely as a tripwire -- if a future edit to tick drops the
  /// precondition, loopInterval fails loudly instead of
  /// boundedDualLeadership failing confusingly one tick past the bound.
  val loopInterval = NODES.forall(n =>
    (alive.get(n) and leading.get(n)) implies
      (clocks.get(n) - fence.get(n) <= FENCE_AFTER + RENEW))

  /// The soft half of sched.lease.at-most-one-leader+3. Two nodes
  /// simultaneously believing they lead IS reachable (the dual-belief
  /// window: a deposed leader that hasn't yet noticed). The property is
  /// that the window is BOUNDED to one loop iteration. That claim splits
  /// into an assumption and a theorem:
  ///
  ///   ASSUMPTION (loopInterval, enforced by tick's precondition): the
  ///   loop runs every RENEW ticks, so a Leading node takes its next
  ///   GET / PUT / maybe_self_fence within RENEW ticks of any state.
  ///
  ///   THEOREM (boundedDualLeadership): in every dual-belief state, the
  ///   stale believer's next loop iteration DISCOVERS the loss -- one of
  ///   its discovery paths is already armed. The window is therefore
  ///   <= one loop iteration. The paths:
  ///     (1) Self-fence: clocks[stale] - fence[stale] >
  ///         STEAL_AFTER - 2*MAX_SKEW. The thief measured > STEAL_AFTER
  ///         of staleness on ITS clock before stealing; the victim's own
  ///         fence age is within 2*MAX_SKEW of that. It self-fences at
  ///         FENCE_AFTER. The distance to its deadline is therefore at
  ///         most 2*MAX_SKEW - (STEAL_AFTER - FENCE_AFTER). With zero
  ///         separation that is 2*MAX_SKEW -- the victim is within
  ///         2*MAX_SKEW ticks of fencing. With separation >=
  ///         RENEW + 2*MAX_SKEW the victim is at least RENEW ticks PAST
  ///         its deadline -- beyond the loopInterval slack, so the tick
  ///         cap has already forced its fence to fire or its clock to
  ///         stop before the thief's steal threshold clears (that is why
  ///         neverDual holds there and this disjunct never needs to
  ///         fire). The 2*MAX_SKEW correction is the price of per-node
  ///         clocks. With MAX_SKEW >= STEAL_AFTER/2 the disjunct is
  ///         vacuous (clocks - fence >= 0 always) and the protocol gives
  ///         no fence guarantee.
  ///     (2) 409: the snapshot in hand is stale, the next PUT gets 409
  ///         Conflict, the loop flips to not-leading (the lose
  ///         transition).
  ///     (3) Observation: the snapshot already shows the new holder; the
  ///         next decide() returns Standby and the loop flips to
  ///         not-leading.
  ///     (4) Deposed by an operator deleting the Lease out from under it
  ///         rather than by a thief that observed it as stale. The
  ///         steal-threshold coupling of disjunct (1) does not apply --
  ///         no thief ever measured this victim's staleness, so its fence
  ///         can be arbitrarily fresh -- and its snapshot was spent at
  ///         its own acquisition, so (2)/(3) have nothing to look at. Its
  ///         window is bounded by its OWN self-fence deadline instead:
  ///         loopInterval caps a Leading node at FENCE_AFTER + RENEW
  ///         ticks past its last round-trip and selfFence is enabled from
  ///         FENCE_AFTER + 1, so the window is at most
  ///         FENCE_AFTER + RENEW (one full fence lifetime) rather than
  ///         2*MAX_SKEW + RENEW. Weaker, but still finite, and the
  ///         generation fence (staleLeaderHasStaleGeneration) is what
  ///         protects correctness inside the window. delVictims is
  ///         cleaned on the victim's next steal, so this disjunct cannot
  ///         mask the discovery obligation of a LATER steal-deposition of
  ///         the same node.
  ///   Path (1) covers the blind victim (no completed write since its
  ///   acquisition, fence aging toward the deadline). Paths (2)/(3) cover
  ///   the connected victim (its GETs hand it a snapshot that exposes the
  ///   loss without touching its fence). A deposed believer's fence NEVER
  ///   refreshes: the only fence-resetting actions are a successful write
  ///   (steal/renewLease -- requires winning the CAS, which a deposed
  ///   believer's stale snapshot cannot) or a 409 (conflict -- which
  ///   makes it not-leading, no longer a believer). Its fence is frozen
  ///   at its last successful write and only ages.
  ///
  /// Why a state invariant and not the temporal [](dual => <>not(dual))
  /// with weak fairness on selfFence: the temporal form is unsound under
  /// a bounded, fairness-free clock. The checker finds a stuttering
  /// counterexample in which the stale believer's clock simply stops one
  /// tick short of its fence deadline -- selfFence is never *enabled*, so
  /// weak fairness never forces it. "Eventually" is the wrong claim for a
  /// bounded model; "the discovery path is already armed" is the right
  /// claim, and it is stronger.
  ///
  /// Deliberately-weakened test (run once during development, NOT in
  /// CI): weakening steal's staleness guard from `> STEAL_AFTER` to
  /// `>= 0` (steal on first observation) produces a counterexample: n1
  /// acquires, n2 observes and steals immediately, n1 is deposed with a
  /// fresh fence (clocks - fence = 0) and no snapshot -- no discovery
  /// path is armed. The invariant is what couples the steal threshold to
  /// the fence threshold: a thief may only steal a lease whose holder is
  /// already at (or within 2*MAX_SKEW - separation of) its own
  /// self-fence deadline.
  val boundedDualLeadership = NODES.forall(m => NODES.forall(n =>
    (leading.get(m) and leading.get(n) and m != n) implies and {
      // In every dual one of the believers actually holds the lease --
      // unless an operator deleted it out from under the holder while a
      // previously-deposed believer still hadn't noticed ITS loss; then
      // neither does and BOTH believers are stale. With no deletion the
      // holder is always the most recent stealer, which is Leading (or
      // crashed, and a crashed node is not a believer).
      or {
        lease.holder == Held(m),
        lease.holder == Held(n),
        lease.holder == NoHolder and deletes > 0,
      },
      // Every stale believer -- the believers that do NOT hold the lease
      // (one of the two normally; both after a deletion) -- has a
      // discovery mechanism armed:
      Set(m, n).filter(x => lease.holder != Held(x)).forall(stale => or {
        // (1) within reach of its own self-fence deadline
        clocks.get(stale) - fence.get(stale) > STEAL_AFTER - 2 * MAX_SKEW,
        // (2)/(3) its snapshot exposes the loss
        and {
          hasSnap(stale),
          or {
            snapRvOf(stale) != lease.rv,
            snapHolderOf(stale) != Held(stale),
          },
        },
        // (4) deposed by a deletion; bounded by its own fence deadline
        delVictims.contains(stale),
      }),
    }))

  /// The bridge to sched.lease.generation-fence+2. When the dual-belief
  /// window is open, the executor's generation fence has to be able to
  /// tell the replicas apart -- the fresh leader's generation must be
  /// strictly greater than the stale believer's. The production
  /// mechanism is on_acquire(transitions)'s fetch_max(leaseTransitions
  /// + 1) plus the write-ahead claim (sched.lease.generation-claim); the
  /// model encodes both in steal: gen[n]' = max(gen[n], lease.gen+2,
  /// genHW+1) and genHW' = that same value.
  ///
  /// FIXED -- this invariant was FALSIFIED by the pre-fix protocol (an
  /// in-memory fetch_add(1) seeded from a PG high-water mark that only
  /// advanced at first dispatch). The counterexample: n1
  /// steals the empty lease (gen 2, seeded from genHW=0), is deposed
  /// before its first dispatch-time PG write, n2 observes the staleness
  /// and steals, seeding from the SAME genHW=0 -- both Leading at gen 2,
  /// and the executor fence cannot tell their WorkAssignments apart. The
  /// window between acquiring a generation and durably recording it was
  /// the hole.
  ///
  /// Two protocol changes closed it, and the model encodes both:
  ///   (a) The generation derives from the lease's transition count
  ///       (lease.gen+2 in steal's target) -- the apiserver bumps
  ///       leaseTransitions atomically with the holder change, so two
  ///       distinct holders can never derive the same generation from
  ///       it. In the counterexample trace, lease.gen was already 1 vs 2
  ///       while the in-memory generations collided at 2 vs 2.
  ///   (b) The write-ahead claim (genHW' advances in steal itself) --
  ///       the generation is durably recorded in PG before dispatch is
  ///       ungated, so a successor's seed always exceeds every
  ///       generation ever handed to dispatch, even one whose holder
  ///       never dispatched anything.
  /// Either change alone closes THIS counterexample; (b) is what
  /// survives Lease-object deletion (which resets lease.gen to 0 and
  /// re-arms the (a) mechanism's failure mode). The Deletion regime
  /// injects exactly that fault and the invariant still holds; reverting
  /// (b) alone -- genHW advancing at a dispatch-time persist instead of
  /// inside steal -- falsifies it under deletion. The claim is the
  /// load-bearing half once the operator can reset the transition count.
  ///
  /// Deliberately-weakened test (run once during development, NOT in
  /// CI): reverting steal's derivation to the pre-fix
  /// max(gen[n]+1, genHW+1) AND moving the genHW update back out of
  /// steal into a separate dispatch-time persist action reproduces the
  /// pre-fix counterexample above. That proves the encoding change is
  /// what makes the invariant pass, not an accident of the surrounding
  /// edits.
  ///
  /// Two cases on whether one of the believers actually holds the lease:
  ///   - A holder exists among the believers (every dual reachable
  ///     without a deletion): the holder is the authorized leader and
  ///     its generation must be STRICTLY GREATEST -- the executor's
  ///     fence converges to the max heartbeat generation it has seen, so
  ///     the stale believer's assignments are rejected and the holder's
  ///     are accepted.
  ///   - No holder (reachable only by deleting the Lease out from under
  ///     a dual -- both believers are now deposed): neither is
  ///     authorized, so there is no "right" believer for the fence to
  ///     prefer. The property degrades to DISTINCTNESS: the fence can
  ///     still order them, the lower one is rejected, and the higher one
  ///     keeps acting until the next real acquisition seeds from genHW+1
  ///     and exceeds both. Distinctness is not a weakening that can hide
  ///     a collision: gen[m] = gen[n] fails both branches. And the
  ///     holderless branch cannot hide a wrong ORDERING either -- the
  ///     deletion does not change any generation, so the holderless
  ///     state's generations are exactly the predecessor state's, which
  ///     the holder-exists branch already checked with the strict
  ///     inequality.
  val staleLeaderHasStaleGeneration = NODES.forall(m => NODES.forall(n =>
    (leading.get(m) and leading.get(n) and m != n) implies
      if (lease.holder == Held(m)) gen.get(n) < gen.get(m)
      else if (lease.holder == Held(n)) gen.get(m) < gen.get(n)
      else gen.get(m) != gen.get(n)))

  // --- 6. actions -------------------------------------------------------

  /// Time. clocks[n] advances independently. Ticks regardless of process
  /// state (alive or not) -- CLOCK_MONOTONIC does not stop for a dead
  /// pod.
  ///
  /// The second conjunct encodes the production loop's tick discipline
  /// (the loopInterval assumption): run_lease_loop() calls
  /// maybe_self_fence() every RENEW_INTERVAL, so a Leading node's clock
  /// cannot advance more than FENCE_AFTER + RENEW past its last
  /// successful round-trip -- the loop would have fenced it first.
  /// Encoding this as a precondition (the state is never generated)
  /// rather than a state constraint (the state is generated, checked
  /// against invariants, and only then discarded) keeps the
  /// production-unreachable states out of invariant checking.
  ///
  /// The third conjunct is the bounded-skew proof assumption
  /// (ClockSkewBound -- a state CONSTRAINT in the TLA+ original, a
  /// precondition here because the TLC backend cannot express
  /// CONSTRAINT). Only the ticking node's new value can newly violate
  /// the bound, so only that direction is checked.
  action tick(n: Node): bool = all {
    clocks.get(n) < MAX_TIME,
    (alive.get(n) and leading.get(n)) implies
      ((clocks.get(n) + 1) - fence.get(n) <= FENCE_AFTER + RENEW),
    NODES.forall(m => (clocks.get(n) + 1) - clocks.get(m) <= MAX_SKEW),
    clocks' = clocks.put(n, clocks.get(n) + 1),
    lease' = lease, alive' = alive, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores,
  }

  /// GET the lease. The obs[n] update follows decide_pure()'s
  /// ObservedUpdate:
  ///   - holder = NoHolder -> Clear (steal now, observed-record
  ///     meaningless)
  ///   - holder = us       -> Keep (we hold it; observed tracks OTHER
  ///     holders)
  ///   - holder = other, same rv we observed -> Keep (clock still
  ///     running)
  ///   - holder = other, new rv (or first observation) -> StartObserving
  ///
  /// fence[n] is UNCHANGED: a bare read does not reset the self-fence
  /// clock. Production resets `last_successful_renew` at exactly two
  /// sites -- the loop init (lib.rs:501, before the node has ever led)
  /// and the `Ok(Ok(result))` arm of the COMPLETE try_acquire_or_renew()
  /// round-trip (lib.rs:525). Every path through that function which
  /// returns Leading ends in an rv-bumping write (create()'s POST or
  /// replace()'s PUT); the paths that complete without a write return
  /// Standby or Conflict, both of which set now_leading=false -- the node
  /// is not a believer afterwards, so its fence value is moot. The
  /// combination "Leading, fresh fence clock, rv unchanged" is therefore
  /// unreachable in production. A model GET that refreshed fence[n] while
  /// leaving the node Leading and the rv unchanged manufactured exactly
  /// that state, and it is what falsified neverDual in the asymmetric
  /// regime: the victim's fence anchor crept forward on every read while
  /// the thief's obs.since anchor stayed at the last rv change, so the
  /// victim's tick cap (anchored to its fence) could outrun the thief's
  /// steal deadline (anchored to the rv) by an unbounded number of reads.
  /// Anchoring both clocks to the same event -- the last completed write
  /// -- is what makes the fence/steal separation argument sound.
  action apiGet(n: Node): bool = all {
    alive.get(n),
    snap' = snap.put(n, Snap(lease)),
    obs' = obs.put(n,
      if (lease.holder == NoHolder) NoObs
      else if (lease.holder == Held(n)) obs.get(n)
      else if (hasObs(n) and obsRvOf(n) == lease.rv) obs.get(n)
      else Observed({ rv: lease.rv, since: clocks.get(n) })),
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    fence' = fence, gen' = gen, genHW' = genHW, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
  }

  /// The write-ahead claim INSERT succeeds: the PG floor advances to the
  /// claimed generation. fetch_max in shape, though every caller passes a
  /// `claimed >= genHW` value when the update is meant to apply (steal's
  /// seeded is always >= genHW + 1; renewLease passes genHW itself off
  /// the acquire edge, a provable no-op).
  ///
  /// A NAMED action (not an anonymous `all` block inside the claim
  /// disjunction) for the same reason every disjunct of `step` is named:
  /// quint's `--mbt` action tracker records the INNERMOST `any`
  /// disjunct's name, so an anonymous block here makes the whole
  /// steal/renewLease step anonymous in generated traces and undrivable
  /// by the model-based tests (rio-lease/src/mbt_tests.rs). The
  /// flattening discipline is recursive: no anonymous `any` disjunct
  /// anywhere reachable from `step`.
  action claimOk(claimed: int): bool = all {
    genHW' = if (claimed > genHW) claimed else genHW,
    claimFailures' = claimFailures,
  }

  /// The write-ahead claim INSERT fails: PG dies between the seed read
  /// and the claim write, and recovery proceeds anyway -- the node still
  /// acquires, still derives gen[n], still leads, but the floor a
  /// successor seeds from does not cover the generation it dispatches
  /// at. The claimFailures increment is the trace marker; the
  /// `claimed > genHW` guard is the non-triviality condition (a claim
  /// that would not have raised the floor cannot fail meaningfully).
  /// Never enabled with MAX_CLAIM_FAILURES = 0.
  action claimFails(claimed: int): bool = all {
    claimFailures < MAX_CLAIM_FAILURES,
    claimed > genHW,
    genHW' = genHW,
    claimFailures' = claimFailures + 1,
  }

  /// The decide()->Steal path: empty-holder OR stale-observed (the
  /// deposed-leader path). gen[n] and genHW are set per
  /// on_acquire(transitions) + seed_generation_from() +
  /// claim_generation() collapsed: all three run before
  /// recovery_complete=true gates dispatch, so the model treats
  /// acquire/seed/claim as one atomic step.
  /// r[sched.recovery.fetch-max-seed+2] (prose reference; the tracey
  /// marker lives at the check wiring point in nix/quint.nix).
  ///
  /// The generation derives from the lease's POST-BUMP transition count:
  /// the PUT writes lease.gen+1 (= leaseTransitions after the holder
  /// change), and on_acquire() fetch_max'es (lease.gen+1)+1 = lease.gen+2
  /// into the atomic (the apiserver bumps the count atomically with the
  /// holder change inside the rv-guarded PUT, so two distinct holders can
  /// never derive the same value from it). The PG floor contributes
  /// genHW+1 (seed_generation_from). Both are fetch_max against the
  /// atomic's current value gen[n] -- nothing unconditionally increments.
  ///
  /// genHW' is the write-ahead claim (sched.lease.generation-claim): the
  /// generation is durably recorded in PG's claims ledger before dispatch
  /// is ungated, so the floor advances at acquisition time. There is no
  /// acquire-to-persist window for a deposed leader to die inside -- the
  /// window the pre-fix counterexample exploited (see the FIXED note at
  /// staleLeaderHasStaleGeneration).
  action steal(n: Node): bool = {
    val target = if (lease.gen + 2 > genHW + 1) lease.gen + 2 else genHW + 1
    val seeded = if (gen.get(n) > target) gen.get(n) else target
    all {
      alive.get(n),
      hasSnap(n),
      // decide()'s steal condition: the lease is unheld, or its holder
      // has been observed at the same rv for longer than the steal
      // threshold.
      or {
        snapHolderOf(n) == NoHolder,
        and {
          snapHolderOf(n) != NoHolder,
          snapHolderOf(n) != Held(n),
          hasObs(n),
          obsRvOf(n) == snapRvOf(n),
          clocks.get(n) - obsSinceOf(n) > STEAL_AFTER,
        },
      },
      casOk(n),
      // State-space bounds, as PRECONDITIONS (action disabled at the
      // ceiling), not saturating clamps -- saturation would let two
      // nodes reach equal generations at MAX_GEN and falsify
      // staleLeaderHasStaleGeneration as a state-space artifact.
      // Disabling loses nothing: a state where the next generation would
      // exceed MAX_GEN is the exploration boundary, same as MAX_TIME for
      // clocks. The rv bound is in casOk.
      lease.gen < MAX_GEN,
      seeded <= MAX_GEN,
      lease' = { holder: Held(n), rv: snapRvOf(n) + 1, gen: lease.gen + 1 },
      leading' = leading.put(n, true),
      gen' = gen.put(n, seeded),
      casRace' = casRaceAfterPut(n),
      acquiredAt' = acquiredAt.put(n, snapRvOf(n)),
      fence' = fence.put(n, clocks.get(n)),
      obs' = obs.put(n, NoObs),       // on_acquire clears observed
      snap' = snap.put(n, NoSnap),    // spent the snapshot
      // A fresh acquisition starts a fresh leadership term: n is no
      // longer the victim of a previous deletion. Without this cleanup a
      // stale delVictims entry would mask a later steal-deposition's
      // discovery obligation.
      delVictims' = delVictims.exclude(Set(n)),
      clocks' = clocks,
      alive' = alive,
      deletes' = deletes,
      restores' = restores,
      // The claim disjunction: the INSERT into the claims ledger either
      // lands (claimOk -- the floor advances; seeded >= genHW+1 always
      // holds here since target's else arm is genHW+1 and seeded >=
      // target) or errors (claimFails -- the ClaimFails fault). Both
      // disjuncts are NAMED actions; see claimOk's doc for why an
      // anonymous block here breaks the model-based tests.
      any {
        claimOk(seeded),
        claimFails(seeded),
      },
    }
  }

  /// The decide()->Renew path: we hold the lease, refresh renew_time.
  /// Bumps rv (the apiserver bumps on every write); does NOT touch the
  /// holder or the transition count.
  ///
  /// leading' = true: a successful renew of a lease we hold means we
  /// lead, whether we already believed it (no-op) or had stopped
  /// believing it (a selfFence false alarm, or a crash/recover cycle
  /// while the lease still named us). The production correspondence is
  /// run_lease_loop()'s acquire arm firing on the was_leading=false ->
  /// now_leading=true edge of a successful renew: is_leader flips back to
  /// true and on_acquire(trans) runs with the UNCHANGED transition count.
  /// Before this conjunct the model under-approximated -- a self-fenced
  /// or recovered replica stayed not-leading forever and the checker
  /// never explored the states where it leads again.
  ///
  /// gen'/genHW' are the SAME on_acquire + recovery-claim collapse that
  /// steal performs (both run before recovery_complete ungates dispatch
  /// on every acquire edge, renew-based or steal-based), with the renew's
  /// UN-bumped transition count:
  ///
  ///   entry  = fetch_max(transitions + 1)      = max(gen[n], lease.gen+1)
  ///            (steal's target is lease.gen+2 because the steal PUT
  ///            bumps the count first; a renew does not)
  ///   seeded = the claim path's floor comparison: a floor ABOVE what the
  ///            lease + the Arc gave us must be exceeded (someone --
  ///            possibly our own pre-crash incarnation -- claimed
  ///            higher); a floor AT it is our own row and is retained.
  ///
  /// The collapse is GATED ON THE ACQUIRE EDGE (not(leading.get(n)) in
  /// the pre-state): production's claim loop lives in
  /// handle_leader_acquired, which runs on the LeaderAcquired actor
  /// message, which the lease loop sends only on the was_leading=false ->
  /// now_leading=true transition. A steady-state renew (was_leading stays
  /// true) never re-runs recovery and never touches the Arc or the claims
  /// ledger. The gate is invisible without fault injection: a Leading
  /// node that can pass casOk has a current snapshot showing itself as
  /// holder, so nobody has stolen -- and therefore nobody has claimed --
  /// since its own acquisition, so genHW = gen[n] = entry = seeded and
  /// the un-gated steady-state branch was already a provable no-op. The
  /// gate exists for the PG fault model: an un-gated update would heal a
  /// FAILED claim (genHW < gen[n] for a Leading node) on the next
  /// steady-state renew, modeling a ledger retry that production only
  /// performs on the next acquire edge.
  ///
  /// The three cases, each pinned by a checker run:
  ///   - selfFence false alarm (never crashed): the Arc still holds our
  ///     claimed generation, which equals genHW while we hold the lease
  ///     -- entry = gen[n], seeded = entry, genHW unchanged. The
  ///     generation is RETAINED and no new claim row is written: the
  ///     idempotent self re-claim. A connectivity blip costs nothing.
  ///   - crash/recover, no deletion: the Arc reset to 1; the transition
  ///     count still encodes our generation (lease.gen + 1 = the value we
  ///     originally derived), and our own claim row sits at exactly that
  ///     floor -- entry = lease.gen + 1 = genHW, seeded = entry.
  ///     RESTORED, no burn. Leaving gen UNCHANGED here instead falsifies
  ///     staleLeaderHasStaleGeneration in the BASE regime (a holder that
  ///     crashes inside a dual-belief window, recovers, and renews would
  ///     lead at gen=1, below the believer it deposed) -- the red test
  ///     for the entry half of this conjunct.
  ///   - crash/recover after a deleteLease: the recreated lease's
  ///     transition count restarted from 0, so it no longer encodes the
  ///     generation we acquired at -- entry under-restores to
  ///     lease.gen + 1 < genHW, and the claim path bumps to genHW + 1 and
  ///     writes a new claim row (production: pg_floor > gen_at_entry ->
  ///     target = pg_floor + 1). Encoding only the entry half falsifies
  ///     staleLeaderHasStaleGeneration in the DELETION regime -- the red
  ///     test for the seeded half.
  ///
  /// acquiredAt is UNCHANGED: it records the rv of the CAS-guarded write
  /// that made this node the HOLDER (the steal), which is what casRace /
  /// atMostOneCASWinner are about -- two acquisitions racing at the same
  /// resourceVersion. A renew is a CAS-guarded write but not an
  /// acquisition; refreshing acquiredAt to the renew's rv would erase the
  /// acquisition history the race detector reads. A crash resets
  /// acquiredAt to NO_ACQ; a recovered holder that re-acquires via renew
  /// therefore has acquiredAt = NO_ACQ, which compares unequal to every
  /// rv and cannot flip casRace.
  action renewLease(n: Node): bool = {
    val entry = if (gen.get(n) > lease.gen + 1) gen.get(n) else lease.gen + 1
    val seeded = if (genHW > entry) genHW + 1 else entry
    val isEdge = not(leading.get(n))
    // What the claim disjunction is offered: the seeded generation on
    // the acquire edge, genHW itself (a provable no-op for claimOk and
    // a disabled precondition for claimFails) off it. Algebraically
    // identical to gating each claim branch on isEdge -- claimOk(genHW)
    // leaves genHW unchanged and claimFails(genHW) fails its
    // `claimed > genHW` guard -- which is how the steady-state renew
    // never re-runs recovery and never touches the ledger.
    val claimTarget = if (isEdge) seeded else genHW
    all {
      alive.get(n),
      hasSnap(n),
      snapHolderOf(n) == Held(n),
      casOk(n),
      // The generation ceiling only constrains the edge branch -- a
      // steady-state renew computes no new generation and must not be
      // disabled at the ceiling.
      isEdge implies (seeded <= MAX_GEN),
      lease' = { ...lease, rv: snapRvOf(n) + 1 },
      leading' = leading.put(n, true),
      gen' = gen.put(n, if (isEdge) seeded else gen.get(n)),
      casRace' = casRaceAfterPut(n),
      fence' = fence.put(n, clocks.get(n)),
      snap' = snap.put(n, NoSnap),
      clocks' = clocks, alive' = alive, obs' = obs,
      acquiredAt' = acquiredAt, deletes' = deletes,
      delVictims' = delVictims, restores' = restores,
      // The same claim disjunction as steal's, restricted to the edge
      // branch via claimTarget -- a steady-state renew never claims, so
      // it cannot fail to claim. The retry-on-the-next-acquire-edge
      // behavior falls out: a failed claim leaves genHW < gen[n], and
      // the NEXT edge renew (or steal) recomputes seeded and offers the
      // ledger the claim again.
      any {
        claimOk(claimTarget),
        claimFails(claimTarget),
      },
    }
  }

  /// The 409 path: the snapshot is stale. Stamps fence (the apiserver
  /// answered). If we were Leading this is an explicit lose -- someone
  /// stole between our GET and PUT. Production distinguishes
  /// 409-on-renew (lose) from 409-on-steal (never led); both end up
  /// not-leading, both reset fence -- the model collapses them.
  action conflict(n: Node): bool = all {
    alive.get(n),
    hasSnap(n),
    lease.rv != snapRvOf(n),
    fence' = fence.put(n, clocks.get(n)),
    leading' = leading.put(n, false),
    snap' = snap.put(n, NoSnap),
    clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
    gen' = gen, genHW' = genHW, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
  }

  /// maybe_self_fence(): a Leading node that has not had a successful
  /// apiserver round-trip in > FENCE_AFTER per its OWN clock flips
  /// is_leader=false locally, without an apiserver write. The production
  /// loop checks this every RENEW_INTERVAL; the model lets it fire any
  /// time the deadline has passed (a superset of the production schedule
  /// -- sound for safety).
  action selfFence(n: Node): bool = all {
    alive.get(n),
    leading.get(n),
    clocks.get(n) - fence.get(n) > FENCE_AFTER,
    leading' = leading.put(n, false),
    clocks' = clocks, lease' = lease, alive' = alive, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores,
  }

  /// Pod crash. Loses ALL in-memory state: the belief, the snapshot, the
  /// observed-record clock, the self-fence clock, the in-memory
  /// generation (the Arc<AtomicU64> -- reset to the production
  /// AtomicU64::new(1) init value, not 0), and the acquiredAt history.
  /// The OS clock (clocks[n]), the apiserver (lease), and PG (genHW)
  /// persist.
  action crash(n: Node): bool = all {
    alive.get(n),
    alive' = alive.put(n, false),
    leading' = leading.put(n, false),
    snap' = snap.put(n, NoSnap),
    obs' = obs.put(n, NoObs),
    fence' = fence.put(n, 0),
    gen' = gen.put(n, 1),
    acquiredAt' = acquiredAt.put(n, NO_ACQ),
    clocks' = clocks, lease' = lease, genHW' = genHW, casRace' = casRace,
    deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
  }

  /// Pod restart. The recovered process has no observation (its first
  /// decide() returns StartObserving and waits a full STEAL_AFTER before
  /// stealing) unless the lease still carries its identity (decide()
  /// returns Renew -- the recovered leader re-acquires its own lease
  /// without contention).
  action recover(n: Node): bool = all {
    not(alive.get(n)),
    alive' = alive.put(n, true),
    clocks' = clocks, lease' = lease, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores,
  }

  /// Operator fault: `kubectl delete lease` destroys the Lease object and
  /// the next replica to GET a 404 recreates it (election.rs::create()).
  /// The two steps collapse into one action: the holder and the
  /// transition count reset, the resourceVersion does NOT -- a recreated
  /// apiserver object takes a fresh rv from the global etcd revision, so
  /// a snapshot of the old incarnation can never satisfy casOk's CAS
  /// against the new one (the deposed-by-deletion holder's next PUT 409s,
  /// exactly as production behaves). Resetting rv here would let a stale
  /// snap spuriously pass the CAS and report an atMostOneCASWinner
  /// violation that no real apiserver admits.
  ///
  /// Everything else is UNCHANGED: the replicas do not observe the
  /// deletion until their next GET; a Leading believer keeps believing
  /// until its next round-trip exposes the loss or it self-fences; PG
  /// (genHW) survives. That asymmetry -- the Lease's epoch source resets
  /// while PG's does not -- is the fault this action injects, and the
  /// write-ahead claim (genHW advancing inside steal) is what survives
  /// it: the next steal of the recreated lease seeds from genHW+1, which
  /// exceeds every generation ever acquired, even one whose holder never
  /// dispatched anything.
  ///
  /// The current holder is recorded in delVictims: it was deposed by an
  /// environmental fault rather than by a thief that observed it as
  /// stale, so boundedDualLeadership's steal-threshold coupling
  /// (disjunct 1) does not apply to it -- see the fourth disjunct there.
  ///
  /// Guards: bounded by MAX_DELETES (one deletion per trace is enough to
  /// exhibit both the post-deletion dual and the post-deletion generation
  /// collision); only a held lease (deleting an unheld lease changes
  /// nothing any invariant constrains and only inflates the state space);
  /// rv headroom for the recreation's fresh resourceVersion.
  action deleteLease = all {
    deletes < MAX_DELETES,
    lease.holder != NoHolder,
    lease.rv < MAX_RV,
    lease' = { holder: NoHolder, rv: lease.rv + 1, gen: 0 },
    deletes' = deletes + 1,
    delVictims' = match lease.holder {
      | NoHolder => delVictims
      | Held(h) => delVictims.union(Set(h))
    },
    clocks' = clocks, alive' = alive, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    acquiredAt' = acquiredAt, casRace' = casRace,
    claimFailures' = claimFailures, restores' = restores,
  }

  /// Operator/infrastructure fault: a PostgreSQL point-in-time restore
  /// regresses the claims ledger AND the assignment history together (the
  /// production floor is GREATEST over both, and a PITR rolls the whole
  /// database back). genHW' = 0 is the worst case -- a restore to before
  /// any claim was ever written; any partial regression is subsumed by it
  /// for the invariants in question (a floor that remembers MORE can only
  /// make a successor's seed HIGHER). Nothing else changes: the apiserver
  /// (the Lease object and its transition count) is a separate system and
  /// survives, which is exactly why this fault alone is survivable -- the
  /// generation derivation falls back to lease.gen + 2, which is
  /// monotonic across stealers as long as nobody resets the transition
  /// count. Guarded by genHW > 0 (restoring an empty ledger changes
  /// nothing) and MAX_RESTORES.
  action pgRestore = all {
    restores < MAX_RESTORES,
    genHW > 0,
    genHW' = 0,
    restores' = restores + 1,
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, fence' = fence, gen' = gen,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
  }

  // --- 7. init ----------------------------------------------------------
  /// gen[n] = 1 mirrors the production `AtomicU64::new(1)`
  /// (rio-scheduler/src/main.rs, rio-controller/src/main.rs). genHW = 0
  /// is an empty PG. lease.gen = 0 is `lease_transitions` before any
  /// holder.
  action init = all {
    clocks' = NODES.mapBy(n => 0),
    lease' = { holder: NoHolder, rv: 0, gen: 0 },
    alive' = NODES.mapBy(n => true),
    leading' = NODES.mapBy(n => false),
    snap' = NODES.mapBy(n => NoSnap),
    obs' = NODES.mapBy(n => NoObs),
    fence' = NODES.mapBy(n => 0),
    gen' = NODES.mapBy(n => 1),
    genHW' = 0,
    acquiredAt' = NODES.mapBy(n => NO_ACQ),
    casRace' = false,
    deletes' = 0,
    delVictims' = NODES.filter(n => false),
    claimFailures' = 0,
    restores' = 0,
  }

  // --- 8. step ----------------------------------------------------------
  // Every disjunct of `step` is a NAMED action with its nondet choices
  // bound directly inside it. Model-based testing (quint-connect) drives
  // the implementation by matching on the action names recorded in a
  // trace and reading their nondet picks as parameters -- an anonymous
  // block in the disjunction cannot be matched. The flattening is
  // semantics-preserving: `nondet n; any { a(n), b(n) }` and
  // `any { nondet n; a(n), nondet n; b(n) }` are the same existential
  // over the same (action, node) pairs (exists distributes over or), so
  // the transition relation -- and therefore the verified state space --
  // is bit-for-bit identical. Re-verify that claim against the
  // distinct-state counts in the introducing commit after any change
  // here.
  //
  // The rule is RECURSIVE: the tracker records the name of the
  // INNERMOST `any` disjunct evaluated during the step, so an anonymous
  // `all { ... }` block inside an `any` nested anywhere under a step
  // disjunct makes the whole step anonymous -- naming only the
  // top-level disjuncts is not enough. The claim disjunction inside
  // steal/renewLease is the precedent: its two branches are the named
  // actions claimOk/claimFails (referenced, not inlined) for exactly
  // this reason, and a generated trace records a steal step as
  // `claimOk`, not `stealAny`. A driver therefore matches on the leaf
  // names; the outer disjunct's nondet picks are still recorded.

  /// One node's clock advances.
  action tickAny = {
    nondet n = NODES.oneOf()
    tick(n)
  }
  /// One node GETs the lease.
  action apiGetAny = {
    nondet n = NODES.oneOf()
    apiGet(n)
  }
  /// One node's decide()->Steal PUT lands.
  action stealAny = {
    nondet n = NODES.oneOf()
    steal(n)
  }
  /// One node's decide()->Renew PUT lands.
  action renewLeaseAny = {
    nondet n = NODES.oneOf()
    renewLease(n)
  }
  /// One node's PUT 409s against a stale snapshot.
  action conflictAny = {
    nondet n = NODES.oneOf()
    conflict(n)
  }
  /// One node's self-fence deadline fires.
  action selfFenceAny = {
    nondet n = NODES.oneOf()
    selfFence(n)
  }
  /// One node's process crashes.
  action crashAny = {
    nondet n = NODES.oneOf()
    crash(n)
  }
  /// One crashed node's process restarts.
  action recoverAny = {
    nondet n = NODES.oneOf()
    recover(n)
  }

  action step = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    selfFenceAny,
    crashAny,
    recoverAny,
    deleteLease,
    pgRestore,
  }
}

// =========================================================================
// The base fault regime: no faults beyond crash/recovery and clock skew.
//
// Constants are chosen for COVERAGE -- the smallest state space that
// still reaches every interleaving class the invariants are about: the
// two-party CAS race, the deposed-leader steal (one full observed-clock
// expiry), the crash/recover cycle, the renew-based re-acquisition (a
// self-fenced or crashed holder whose lease nobody stole leads again
// without a steal), and one clock-tick of skew.
//
// STATE-SPACE BASELINE: the exact distinct/generated state counts, the
// BFS depth, and the wall-clock are MEASUREMENTS -- they live in the
// check's output transcript (regenerated on every run, never stale) and
// in the commit message that introduced or last re-measured this module
// (immutable, true by construction), never in this comment. The durable
// structural claim, verified at port time: the port's distinct-state
// count is exactly |NODES|! times the TLA+ predecessor's (which used
// the symmetry reduction the TLC backend cannot express), minus the
// handful of states that are their own permutation image. That exact
// ratio is the structural-identity evidence for the port -- the two
// state spaces are the same space, one folded by symmetry and one not.
// Re-verify the ratio (and re-record the figures in the commit message)
// after any change to the model or the constants.
//
// What staying at these constants loses:
//   - NODES={n1,n2} (not 3): no spectator replica. Every contention
//     scenario is two-party. The CAS argument is pairwise (any k-party
//     race contains a 2-party race as a prefix), so this is acceptable
//     for atMostOneCASWinner; a three-party boundedDualLeadership
//     violation would need two simultaneously-stale ex-leaders, which
//     requires two prior leadership cycles -- partially covered by
//     MAX_TIME=8 (2 cycles). The 3-node space was measured (against the
//     TLA+ predecessor) to be intractable for exhaustive checking: the
//     third node multiplies the per-state combinations (its own clock,
//     belief, gen, obs, snap, fence, acquiredAt), and the BFS queue was
//     still growing when the measurement run was killed.
//   - MAX_TIME=8 (not 12): 2 leadership cycles, not 3. Acquire ->
//     depose -> re-acquire is reachable; a third handoff is not.
//   - FENCE_AFTER=STEAL_AFTER=3: ZERO separation -- this regime models
//     the DEGRADED case (the pre-asymmetric-TTL production constants, or
//     a deployment whose clock skew exceeds the margin). Dual belief is
//     reachable here and boundedDualLeadership is the operative
//     property. The Asymmetric regime models the healthy case
//     (separation > 2*MAX_SKEW) where neverDual holds instead. The 3:1
//     deadline/RENEW ratio mirrors production LEASE_TTL=15s /
//     RENEW_INTERVAL=5s while shrinking the observed-clock state space.
//   - MAX_DELETES = MAX_CLAIM_FAILURES = MAX_RESTORES = 0: every
//     operator/infrastructure fault is DISABLED. Keeping the base regime
//     fault-free keeps the core-protocol proof claim separately
//     falsifiable (a regression in the core protocol shows up here, not
//     buried in the larger fault-injection runs).
//
// NON-VACUITY at these constants (re-verify after any constant change;
// never shrink past the point where these checks stop firing):
//   - Dual (two simultaneous believers) is REACHABLE: adding `neverDual`
//     to THIS regime's invariant list produces a shallow counterexample
//     (verified at port time; the BFS depth at which it fires is a
//     measurement -- see the introducing commit). neverDual is a
//     permanent definition, checked for real only in the Asymmetric
//     regime where the separation makes it hold. boundedDualLeadership's
//     and staleLeaderHasStaleGeneration's shared antecedent is
//     satisfiable here.
//   - boundedDualLeadership is FALSIFIABLE: weakening steal's staleness
//     guard from `> STEAL_AFTER` to `>= 0` produces a shallow
//     counterexample (verified at port time) -- a node deposed
//     immediately after acquiring has a fresh fence and no discovery
//     path armed. The `> STEAL_AFTER` steal threshold is exactly what
//     arms discovery.
//   - staleLeaderHasStaleGeneration is FALSIFIABLE: reverting steal's
//     generation derivation to the pre-fix max(gen[n]+1, genHW+1) and
//     moving the genHW update back to a dispatch-time persist action
//     reproduces the generation collision (both believers at gen 2).
//   - renewLease's generation update is LOAD-BEARING, both halves:
//     restoring leading[n] while leaving gen UNCHANGED (no on_acquire on
//     the renew-based acquire edge) falsifies
//     staleLeaderHasStaleGeneration -- a holder that crashes inside a
//     dual-belief window, recovers, and renews its still-valid lease
//     would lead at the reset gen=1, below the stale believer it
//     deposed.
//   - Note: removing selfFence from step does NOT violate
//     boundedDualLeadership -- the invariant is a state property about
//     discovery being ARMED, not a temporal property about the fence
//     FIRING. The steal-threshold weakening is the correct falsification
//     lever; the fence-firing obligation is loopInterval's (enforced by
//     tick's precondition).
// =========================================================================
module leaderElectionBase {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 3,
    STEAL_AFTER = 3,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0
  ).*

  // --- deterministic protocol runs ---------------------------------------
  // The executable versions of the protocol's known-critical scenarios:
  // each is a counterexample trace from the model's development history
  // (or the happy-path twin of one), pinned as a regression test.
  // `quint test --main=leaderElectionBase --match=<run>` replays them;
  // the MBT harness (quint-connect) replays the same named runs against
  // the rio-lease implementation and diffs the projected state after
  // every step. They do not change the verified state space (runs are
  // not part of `step`).
  //
  // Tick choreography: tick(n)'s skew precondition keeps every clock
  // within MAX_SKEW = 1 of every other, so advancing one node's clock to
  // T requires alternating ticks that drag the other node's clock to at
  // least T - 1. tick(n)'s loop-discipline precondition caps a LEADING
  // node's clock at fence + FENCE_AFTER + RENEW; with fence = 0 that is
  // exactly clock 4, which is also the first instant its self-fence
  // deadline (clock - fence > FENCE_AFTER) and a follower's steal
  // deadline (clock - since > STEAL_AFTER) have both passed. Every run
  // below that needs a deadline to expire ticks to exactly that
  // boundary.

  /// The CAS race -- the hard half of at-most-one-leader. Both nodes GET
  /// the empty lease at rv 0; n1's PUT lands first and bumps the rv to 1;
  /// n2's PUT against its now-stale rv-0 snapshot is the 409 path (the
  /// model's `conflict` action -- `steal(n2)` is *disabled* because casOk
  /// requires the snapshot rv to match the live rv). Exactly one writer
  /// per resourceVersion wins.
  run casRaceRun =
    init
      .then(apiGet("n1"))
      .then(apiGet("n2"))
      .then(steal("n1"))
      .then(conflict("n2"))
      .expect(all {
        leading.get("n1"),
        not(leading.get("n2")),
        lease.holder == Held("n1"),
        lease.rv == 1,
        gen.get("n1") == 2,
        not(casRace),
      })

  /// The deposed-leader steal. n1 acquires and goes silent (no further
  /// round-trips); n2 observes the lease at rv 1 and watches it not
  /// change for STEAL_AFTER + 1 ticks; n2 steals. n1 has not yet
  /// discovered the deposition (its next round-trip would 409, its
  /// self-fence deadline has passed but not yet fired) -- this is the
  /// dual-belief state boundedDualLeadership bounds -- but the
  /// generation fence already tells the two apart: n2's generation is
  /// strictly greater.
  run deposedLeaderStealRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(apiGet("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(steal("n2"))
      .expect(all {
        leading.get("n2"),
        leading.get("n1"),          // the dual-belief window is open...
        gen.get("n2") > gen.get("n1"), // ...and the generation fence closes it
        lease.holder == Held("n2"),
        not(casRace),
      })

  /// The self-fence false alarm -- the idempotent re-acquisition. n1
  /// acquires, then cannot reach the apiserver for FENCE_AFTER + 1 ticks
  /// and self-fences (stops believing, no apiserver write). Connectivity
  /// returns; its lease was never stolen; its renew succeeds. It leads
  /// again at the SAME generation -- the lease's transition count did not
  /// move, so on_acquire's fetch_max is a no-op and the claim path finds
  /// its own row at the floor. A connectivity blip costs nothing: no
  /// generation burn, no fencing of its own in-flight work.
  run selfFenceFalseAlarmRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1"))
      .then(selfFence("n1"))
      .then(apiGet("n1"))
      .then(renewLease("n1"))
      .expect(all {
        leading.get("n1"),
        gen.get("n1") == 2,         // the SAME generation -- no burn
        genHW == 2,                 // no new claim row either
        lease.holder == Held("n1"),
        lease.gen == 1,             // the transition count never moved
      })

  /// The crash-recover-renew re-acquisition. n1 acquires, crashes (its
  /// in-memory generation Arc resets to 1), recovers, GETs its
  /// still-valid lease, and renews. It leads again at its PRE-CRASH
  /// generation: the lease's transition count (lease.gen + 1 = 2) still
  /// encodes the generation it originally derived, and its own claim row
  /// sits at exactly that floor -- restored, not burned, and not the
  /// reset in-memory value of 1.
  run crashRecoverRenewRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(crash("n1"))
      .then(recover("n1"))
      .then(apiGet("n1"))
      .then(renewLease("n1"))
      .expect(all {
        leading.get("n1"),
        gen.get("n1") == 2,         // restored from the transition count
        lease.holder == Held("n1"),
      })
}

// =========================================================================
// The Lease-object-deletion fault regime: MAX_DELETES = 1, everything
// else identical to leaderElectionBase (see that module for the
// coverage-vs-cost argument behind the shared constants).
//
// The fault (the deleteLease action; see its doc comment for the
// mechanics): `kubectl delete lease` is a documented operational remedy
// for a stuck election in the Kubernetes ecosystem. The recreated
// object's leaseTransitions counter restarts from 0 -- destroying the
// epoch source the generation derivation reads -- while its
// resourceVersion keeps counting (a recreated apiserver object takes a
// fresh rv from the global etcd revision; resetting the model's rv
// would let a stale snapshot pass the CAS and report a violation no
// real apiserver admits). PG survives. That asymmetry -- the Lease's
// epoch source resets while PG's does not -- is the one fault that
// re-arms the generation-collision failure mode the transition-count
// derivation closed.
//
// What this regime proves: the write-ahead claim (genHW advancing
// inside steal/renewLease's acquire edge, before dispatch is ungated)
// is what survives the deletion. The next steal of the recreated lease
// seeds from the PG floor, which covers every generation ever
// ACQUIRED -- not just every generation that happened to be persisted
// on an assignment row before its holder was deposed.
//
// RED-FIRST PROCEDURE (re-verify after any change to steal's or
// renewLease's generation/claim encoding; the violation depths and the
// state counts are measurements and live in the introducing commit's
// message, not here):
//   - The claim is LOAD-BEARING under deletion: revert steal's genHW
//     update to leave the floor unchanged (the pre-claim protocol,
//     where the floor advanced only at a lazy dispatch-time persist a
//     deposed leader never reaches) and staleLeaderHasStaleGeneration
//     is falsified with a shallow trace -- steal, delete, steal: both
//     believers derive the same generation because both acquisitions
//     see the recreated lease's reset transition count and the same
//     stale floor. If a constant change makes the claim-reverted model
//     pass, the constants no longer reach the deletion collision and
//     must be revisited.
//   - boundedDualLeadership's fourth disjunct (the delVictims arm) is
//     LOAD-BEARING: deleteLease deposes a holder that has no snapshot
//     and a fresh fence, and the empty-holder steal branch lets a
//     second node acquire immediately (no STEAL_AFTER observation
//     wait) -- a dual with none of the steal-path discovery mechanisms
//     armed. Removing the fourth disjunct falsifies the invariant.
//   - renewLease's claim-path arm (seeded = genHW + 1 when the floor
//     exceeds what the lease and the Arc gave us) is LOAD-BEARING here
//     and only here: after a deletion the recreated lease's transition
//     count no longer encodes a recovered holder's generation, so the
//     entry half alone under-restores it to the deletion victim's
//     generation. See renewLease's doc comment, case 3.
//   - staleLeaderHasStaleGeneration's holder-exists/holderless split
//     exists for this regime: deleting the Lease out from under an
//     existing dual produces a state where NEITHER believer holds it.
// =========================================================================
module leaderElectionDeletion {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 3,
    STEAL_AFTER = 3,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 1,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0
  ).*
}

// =========================================================================
// THE HEALTHY REGIME: the asymmetric-TTL split. FENCE_AFTER = 2 and
// STEAL_AFTER = 5 (separation 3); every other constant identical to
// leaderElectionBase (see that module for the coverage-vs-cost argument
// behind the shared constants). No faults beyond crash/recovery and
// clock skew -- the separation, not a fault budget, is what
// distinguishes this regime.
//
// The base and Deletion regimes keep ZERO separation and model the
// DEGRADED case: dual belief is reachable there and
// boundedDualLeadership (every dual-belief state has a discovery
// mechanism armed) is the operative property. This regime models what
// the asymmetric-TTL change gives production (SELF_FENCE_AFTER =
// LEASE_TTL - FENCE_MARGIN = 11s, STEAL_AFTER = LEASE_TTL + FENCE_MARGIN
// = 19s), and the operative property upgrades to neverDual: NO
// dual-belief state is reachable at all.
//
// WHY IT HOLDS -- the separation condition:
//
//   neverDual holds  iff  STEAL_AFTER - FENCE_AFTER >= RENEW + 2*MAX_SKEW
//
// A thief may steal only after measuring > STEAL_AFTER of staleness on
// its own clock since the last rv change. The victim's fence is anchored
// to the same rv change (its last completed write -- a bare read does
// not move the anchor; see apiGet), so the victim's fence age is within
// 2*MAX_SKEW of the thief's measurement. The victim self-fences at
// FENCE_AFTER and loopInterval gives it at most RENEW more ticks past
// the deadline before the tick cap forces the fence to fire or the clock
// to stop. The steal threshold is therefore unreachable while the victim
// still believes iff STEAL_AFTER - 2*MAX_SKEW >= FENCE_AFTER + RENEW.
// The RENEW term is the victim's fence-CHECK latency (the loop only
// looks at the deadline once per tick) -- the textbook margin condition
// that omits it is wrong, and the model is where the omission was
// caught. The production translation: 2*FENCE_MARGIN >= RENEW_INTERVAL +
// 2*clock_skew, i.e. 8s >= 5s + 2*skew -- a 1.5s one-sided clock-skew
// budget, encoded as rio-lease's compile-time assertion. A clock pause
// longer than that re-opens the window, which is the impossibility
// result the generation fence backstops.
//
// BOUNDARY PROCEDURE (re-verify after any change to the tick/fence/steal
// encoding or these constants; the violation depths are measurements and
// live in the introducing commit's message, not here):
//   - Separation 3 (this module: STEAL_AFTER = 5): 3 >= RENEW +
//     2*MAX_SKEW = 3 -- neverDual HOLDS, with equality. The margin is
//     exactly sufficient.
//   - Separation 2 (set STEAL_AFTER = 4): 2 < 3 -- neverDual MUST be
//     VIOLATED. The boundary from below: one tick less separation and a
//     dual-belief state is reachable.
//   - Separation 0 (set STEAL_AFTER = 2): neverDual MUST be VIOLATED.
//     The degraded regime reproduced inside this instantiation.
//   If either weakened run stops producing its counterexample, the
//   constants no longer reach the dual-belief interleavings and
//   neverDual is vacuously true -- revisit before trusting the green
//   run.
//
// NON-VACUITY at these constants (re-verify after any constant change):
//   - The margin is LOAD-BEARING: the separation-2 and separation-0 runs
//     above. neverDual holds because of the separation, not because the
//     constants made contention unreachable.
//   - Steals are still REACHABLE at STEAL_AFTER = 5: the
//     atMostOneAcquisition witness (lease.gen < 2) is violated -- the
//     lease's transition count reaches 2 only via a steal of a
//     previously-held lease, so the contention scenario neverDual
//     constrains actually happens within MAX_TIME = 8 (a node that GETs
//     at t=0 needs its observation to age past STEAL_AFTER = 5, so the
//     steal fires in the last three ticks). neverDual is not vacuously
//     true. Machine-checked: quint-leader-election-witness-second-
//     acquisition (nix/quint.nix) fails if this re-verification ever
//     stops producing its violation.
// =========================================================================
module leaderElectionAsymmetric {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0
  ).*
}

// =========================================================================
// The PostgreSQL fault regime: both PG-side faults enabled, the
// Lease-deletion fault DISABLED, every other constant identical to
// leaderElectionBase (see that module for the coverage-vs-cost argument
// behind the shared constants; the symmetric FENCE_AFTER = STEAL_AFTER
// keeps this regime comparable to the base and Deletion regimes -- the
// PG faults are orthogonal to the TTL asymmetry).
//
// The faults:
//   - ClaimFails (MAX_CLAIM_FAILURES = 1): the claim INSERT errors
//     between the seed read and the claim write and recovery proceeds
//     anyway -- the production proceed-on-failure path
//     (rio_scheduler_generation_claim_failed_total). The acquiring node
//     still derives its generation and still leads, but the floor a
//     successor seeds from never learns the generation it dispatches at.
//     Encoded as the second disjunct of steal's (and renewLease's edge
//     branch's) genHW update; the claimFailures counter is the trace
//     marker.
//   - PgRestore (MAX_RESTORES = 1): a point-in-time restore regresses
//     the floor (the claims ledger and the assignment history together)
//     to 0. The pgRestore action.
//
// What this regime proves -- THE LEASE AND THE PG LEDGER ARE REDUNDANT
// EPOCH SOURCES: every invariant holds with both PG faults injected.
// steal's generation target is max(lease.gen + 2, genHW + 1); when the
// floor term lies (a failed claim left it behind, a restore zeroed it),
// the lease-derived term still increases strictly across successive
// stealers, because only a steal bumps the transition count and nothing
// in the PG fault set resets it. Each PG fault alone is survivable. A
// generation collision requires destroying BOTH epoch sources -- a PG
// fault in conjunction with a Lease deletion -- and those conjunctions
// are exactly the documented, accepted residuals of the
// proceed-on-failure choice and of relying on PG as the post-deletion
// backstop.
//
// CONJUNCTION-EVIDENCE PROCEDURE (the boundary of the documented
// residuals; temporary edits to THIS module's constants, never CI
// checks; re-verify after any constant or encoding change -- the
// violation depths and trace summaries are measurements and live in the
// introducing commit's message, not here):
//   - DeleteLease + ClaimFails (MAX_DELETES = 1, MAX_CLAIM_FAILURES = 1,
//     MAX_RESTORES = 0): staleLeaderHasStaleGeneration MUST be VIOLATED
//     with a shallow steal-delete-steal trace -- the first steal's claim
//     fails so the floor never learns its generation, the deletion
//     resets the lease's transition count, and the second steal derives
//     the same generation from the reset lease and the stale floor.
//   - DeleteLease + PgRestore (MAX_DELETES = 1, MAX_CLAIM_FAILURES = 0,
//     MAX_RESTORES = 1): VIOLATED -- the first steal's claim succeeds,
//     the deletion resets the lease's epoch source, the restore wipes
//     the PG one, and the second steal has nothing left to be monotonic
//     over.
//   If either conjunction run ever PASSES, the constants no longer reach
//   the collision and must be revisited -- that is this regime's
//   non-vacuity evidence (the faults are load-bearing in conjunction
//   with the deletion, so their single-fault survival is not an artifact
//   of the faults never firing). If the MAX_DELETES = 0 run ever FAILS,
//   a PG fault alone breaks the fence with no operator action: a new
//   protocol finding, not a tuning problem.
// =========================================================================
module leaderElectionPgFaults {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 3,
    STEAL_AFTER = 3,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 8,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 1,
    MAX_RESTORES = 1
  ).*
}
