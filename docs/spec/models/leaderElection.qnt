// -*- mode: Bluespec; -*-
// quint-policy-latches: evidenceFabricated, evidenceConsumed
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
// deletion. The claim-target rule is modeled with ledger-row ownership
// (seedFor / genHWHolder), so the retain-on-own-claim re-acquisition the
// fetch-max-seed and generation-claim rules mandate is INSIDE the
// verified space (the noRetainAfterDeletion witness check is the
// standing evidence); the post-claim leadership confirmation that gates
// an under-vouched target (sched.recovery.bump-confirm) is
// implementation-side sequencing, tested in Rust, not modeled here. See
// each regime module's header for its coverage-vs-cost
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
//               dispatch until the seed AND the claim have both run,
//               and a claim target the durable PG floor cannot vouch
//               for additionally requires a post-claim Leading round --
//               sched.recovery.bump-confirm; see the steal action doc)
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
// regimes need the claims-ledger mock and genHW in the projection);
// splitting acquisition from seed/claim so the deposed-unaware
// recovery interleaving (the window sched.recovery.bump-confirm closes
// at the implementation level) is modeled directly instead of being
// collapsed into one atomic step.
module leaderElection {
  // --- 1. types ---------------------------------------------------------
  type Node = str
  /// The Lease object's holderIdentity: empty or a node.
  type Holder = | NoHolder | Held(Node)
  /// The Lease object as the apiserver stores it: holder, resourceVersion,
  /// leaseTransitions (the `gen` field -- the transition count, NOT a
  /// node's in-memory generation).
  /// `rv` mirrors metadata.resourceVersion: bumped by EVERY write,
  /// protocol or foreign — it stays the CAS guard. `content` mirrors
  /// the holder-authored spec content (holderIdentity + renewTime
  /// bytes, abstracted to a counter): bumped by protocol writes only.
  /// The observed record and the evidence cursor key on `content`
  /// (production decide()/last_fetched_renew_time); a foreign bumpRv
  /// moves `rv` alone.
  type LeaseRec = { holder: Holder, rv: int, content: int, gen: int }
  /// A node's last GET'd copy of the lease (TLA+ snap[n], NULL when the
  /// node has no snapshot in hand / the snapshot was spent).
  type SnapState = | NoSnap | Snap(LeaseRec)
  /// A node's observed-record staleness clock (TLA+ obs[n]): the rv it is
  /// watching and the local-clock instant it started watching.
  type ObsState = | NoObs | Observed({ key: int, since: int })
  /// An in-flight renew round-trip whose apiserver write committed at
  /// SEND but whose response has not been read yet (the loop parked on
  /// the await). `anchor` is the attempt-start instant on the node's
  /// clock — what production's RenewAnchor captures before the await.
  /// Only the suspend fault (MAX_SUSPENDS > 0) ever creates one.
  type PendingState = | NoPending | PendingRead({ anchor: int })
  /// The cancelled-write ledger entry (production UnconfirmedPut,
  /// rio-lease/src/lib.rs): a transmitted-but-unconfirmed lease
  /// write, kept at the OLDEST anchor until own-commit evidence (or a
  /// completed round, wholesale) consumes it. anchor is the attempt
  /// instant on the node's clock -- anchor <= send <= commit.
  type UnconfState = | NoUnconf | Unconf({ anchor: int })
  /// A pending cooperative step-down request: `stamp` is what the
  /// serve comparison reads (transition count or instance, per
  /// STEP_DOWN_COUNT_KEYED); `inst` is the TRUE per-acquire instance
  /// that filed it -- the staleStepDownServed latch's ground truth.
  type StepDownState = | NoStepDown | StepDownReq({ stamp: int, inst: int })

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
  /// Split renews per trace (0 = disabled): a steady-state renew whose
  /// apiserver write commits at send while the response read is
  /// arbitrarily delayed — the loop parked on the await, i.e. a host
  /// suspend (or long stall) straddling an in-flight round-trip. While
  /// a read is pending the node takes NO actions (the production loop
  /// is parked), which exempts it from the loop-discipline tick cap —
  /// the honest statement of bug_096: the loop-interval assumption
  /// never held across a parked await.
  const MAX_SUSPENDS: int
  /// Graceful process exits per trace (0 = disabled): SIGTERM — the
  /// loop breaks, runs the release gate, and the process never comes
  /// back (recover is guarded). Exercises the belief/hold split
  /// (bug_387): the release fires iff the node HELD the lease
  /// unsuperseded, regardless of the belief the self-fence cleared.
  const MAX_SHUTDOWNS: int
  /// Dropped writes per trace (0 = disabled): a renew-shaped PUT that
  /// COMMITS at the apiserver while the renew composition fails after
  /// transmission -- the mid-band act deadline of merged_bug_303 (the
  /// fetch fits the phase budget, the act's response does not). The
  /// loop is NOT parked (contrast MAX_SUSPENDS): the round resolves
  /// as an error, no clock is stamped, belief is untouched (decay is
  /// selfFence's job), and the committed write sits in the
  /// UnconfirmedPut ledger until own-commit evidence consumes it.
  /// Distinct from MAX_SUSPENDS' split renew: there the response
  /// arrives late (renewReadOk); here it never arrives at all.
  const MAX_DROPS: int
  /// Foreign non-protocol Lease writers (annotation/label patches by
  /// other tooling): each bumpRv bumps the apiserver resourceVersion
  /// WITHOUT touching the holder-authored spec content. 0 disables the
  /// action entirely (the pre-existing regimes' transition relations
  /// and state spaces are unchanged; re-measured at the introducing
  /// commit). merged_bug_180's fault axis.
  const MAX_FOREIGN_BUMPS: int
  /// Calibration hook for the merged_bug_180 twin: key the observed
  /// record and the evidence cursor on the raw resourceVersion (the
  /// pre-fix production code) instead of the holder-authored content.
  /// false everywhere except the lease-180-rv-keyed falsify twin.
  const OBS_KEY_RV: bool
  /// The blind-holder-window bound (global ticks) blindHolderBounded
  /// asserts in dropped-write regimes; bind 0 wherever MAX_DROPS = 0
  /// (the ghost is frozen there). The value is a calibrated
  /// measurement -- see the introducing commit and the droppedWrite
  /// module comment.
  const BLIND_HOLDER_BOUND: int
  /// Gate for the holder-evidenced-lose plane (bughunt-4 S2, signed
  /// Q3 / sched.lease.holder-evidenced-lose): when true, a believing
  /// node's 409 defers one round instead of losing, the lose edge
  /// demands holder evidence (loseObserved) or deferral exhaustion,
  /// and the conflictDeferredV ghost tracks the pending deferral.
  /// false in every pre-existing regime: their transition relations
  /// and state spaces are unchanged (re-measured at the introducing
  /// commit -- the trackHeld discipline).
  const HOLDER_EVIDENCE_LOSE: bool
  /// Calibration hook for the merged_bug_085 twin: restore the
  /// pre-fix immediate-lose-on-first-409 inside the holder-evidence
  /// world. The blindConflictLose latch (judged on the deferral
  /// ground truth, independent of this toggle's behavior) fires only
  /// under it. false everywhere except the
  /// lease-085-blind-conflict-lose falsify twin.
  const CONFLICT_IMMEDIATE_LOSE: bool
  /// Cooperative step-down requests per trace (0 = the plane is
  /// disabled and its vars are frozen -- pre-existing regimes
  /// unchanged). The production sched.recovery.step-down request/
  /// serve pair, added for the merged_bug_128 instance-epoch law.
  const MAX_STEP_DOWNS: int
  /// Calibration hook for the merged_bug_128 twin: stamp step-down
  /// requests with the lease TRANSITION COUNT (the pre-fix
  /// production code) instead of the per-acquire instance, and skip
  /// the acquire-edge clear. The same-count re-acquire then serves a
  /// stale demotion -- staleStepDownServed (judged on the true
  /// filing instance, independent of the keying) fires. false
  /// everywhere except the lease-128-count-keyed-stepdown twin.
  const STEP_DOWN_COUNT_KEYED: bool

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
  /// Owner of the claims-ledger row at genHW (NoHolder when the ledger
  /// is empty). What the implementation reads as own_claim_at_our_gen:
  /// the retain-on-tie decision is conditioned on the floor row being
  /// OURS, not merely on the floor value. Regressed to NoHolder by
  /// pgRestore together with genHW.
  var genHWHolder: Holder
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
  /// History flag: steal's own-row-tie retain branch has fired (the
  /// post-deletion same-epoch re-acquire that fetch-max-seed /
  /// generation-claim mandate). Model-only bookkeeping read by the
  /// noRetainAfterDeletion witness; never read by an action guard.
  var stealRetained: bool
  /// History flag: a steal's claim target had to exceed a tied foreign
  /// floor (genHW equal to the entry generation under another holder's
  /// row; the not-equal form also covers the ownerless tie, unreachable
  /// at a tie in practice -- "not our row => bump" is the intended
  /// claim-target semantics, so the breadth is deliberate). Model-only
  /// bookkeeping read by the noForeignTieBumpAfterDeletion witness;
  /// never read by an action guard.
  var stealBumpedForeignTie: bool
  /// History flag: a renew-edge re-acquire's claim target had to exceed
  /// a claims-ledger floor strictly ABOVE the entry generation (seedFor's
  /// first arm -- renewLease case 3, the post-deletion crash/recover
  /// restore the deletion-regime header calls load-bearing). Tracks the
  /// renew-edge instance only; the steal-side instance of the same arm is
  /// deliberately not tracked by this flag. Model-only bookkeeping read
  /// by the noFloorBumpAfterDeletion witness; never read by an action
  /// guard.
  var renewBumpedAboveEntry: bool
  /// In-flight split renew per node (suspend fault only; constant
  /// NoPending at MAX_SUSPENDS = 0).
  var pending: Node -> PendingState
  /// LeaseStanding.held_unsuperseded: acquired and not since observed
  /// a completed round resolving not-leading. Tracked only when the
  /// shutdown fault is enabled (trackHeld), so the existing regimes'
  /// state spaces are untouched. Set on steal/renewLease, cleared on
  /// conflict / observing a non-own holder / crash; UNTOUCHED by
  /// selfFence — the fix encoded.
  var held: Node -> bool
  /// Process exited gracefully (terminal; recover is guarded on it).
  var exited: Node -> bool
  /// Split renews so far (0..MAX_SUSPENDS).
  var suspends: int
  /// Graceful exits so far (0..MAX_SHUTDOWNS).
  var shutdowns: int
  /// History latch: a node exited while the apiserver still named it
  /// holder and the release did not run. gracefulHandover = its
  /// negation.
  var releaseSkippedWhileHolder: bool
  /// History latch: a split renew's read happened more than FENCE_AFTER
  /// past its anchor — the suspend straddled the in-flight round-trip.
  /// Read by the noSuspendStraddle witness.
  var suspendStraddled: bool
  /// History latch: a shutdown ran on a node that still held the lease
  /// at the apiserver but no longer believed (post-self-fence) — the
  /// fence-then-SIGTERM shape. Read by the noFenceThenExit witness.
  var fenceThenExit: bool
  /// Per-node cancelled-write ledger (in-memory: a crash loses it --
  /// which is exactly why evidence can never fire post-crash at a
  /// reset generation). Frozen all-NoUnconf at MAX_DROPS = 0.
  var unconfirmed: Node -> UnconfState
  /// Per-node evidence cursor: the rv of the last COMPLETED fetch
  /// (production last_fetched_rv; -1 = none). Updates are gated on
  /// MAX_DROPS > 0 so the four pre-existing regimes' state spaces
  /// stay byte-identical (re-measured at the introducing commit).
  var lastFetchedKey: Node -> int
  /// Ghost: global ticks accumulated while SOME alive node is the
  /// EVIDENCE-STARVED BLIND HOLDER -- the lease names it, it has
  /// stopped believing (fenced), and own-commit evidence of its
  /// transmitted write is OBSERVABLE (entry pending, rv moved past
  /// its cursor) yet unobserved. This is merged_bug_303's failure
  /// window stated as safety: pre-fix it never closes (the livelock,
  /// one cycle of it); with the evidence rule the forcing cap closes
  /// it within one poll. Deliberately NOT "no believer exists":
  /// global leaderlessness is a liveness property the model's
  /// candidates are not poll-forced to close (a standby's steal is
  /// never obligatory), and asserting it as state would be vacuous
  /// or false. Frozen 0 at MAX_DROPS = 0.
  var blindHolderFor: int
  /// Dropped writes so far.
  var drops: int
  /// Foreign non-protocol writes so far. Frozen 0 at
  /// MAX_FOREIGN_BUMPS = 0 (the action is disabled).
  var foreignBumps: int
  /// Ghost cursor: the lease CONTENT at this node's last completed
  /// fetch (-1 = none) — the ground truth the fabrication latch is
  /// judged against, independent of what the evidence cursor keys on.
  /// Updated only at MAX_FOREIGN_BUMPS > 0 (frozen all -1 otherwise:
  /// without foreign writers content and rv never diverge and the
  /// latch below is identically false — byte-identical pre-existing
  /// regimes).
  var lastFetchedContent: Node -> int
  /// History latch (writer: fetchObservesOwnCommit): own-commit
  /// evidence was consumed while the holder-authored CONTENT had NOT
  /// moved past this node's last completed fetch — the consumed
  /// "evidence" was foreign rv churn, not a committed write of ours
  /// (merged_bug_180's fabrication). Frozen at MAX_FOREIGN_BUMPS = 0.
  var evidenceFabricated: bool
  /// History latch (writer: fetchObservesOwnCommit): own-commit
  /// evidence was consumed at least once while foreign bumps were
  /// enabled — the non-vacuity witness for the foreign-rv regime.
  /// Frozen at MAX_FOREIGN_BUMPS = 0.
  var evidenceConsumed: bool
  /// Q3 deferral ghost (production conflict_deferred): a believing
  /// 409 was observed and its resolution is owed to the next
  /// completed read. Frozen all-false at HOLDER_EVIDENCE_LOSE =
  /// false.
  var conflictDeferredV: Node -> bool
  /// History latch: a believing node lost on a FIRST 409 with no
  /// holder evidence and no exhausted deferral -- the pre-fix blind
  /// lose. Writable only under CONFLICT_IMMEDIATE_LOSE (the twin
  /// toggle); loseRequiresHolderEvidence is its negation.
  var blindConflictLose: bool
  /// Ghost (merged_bug_002/bug_002): a resolving action -- own-commit
  /// fetch, the evidence-free holder=self act-failed read, self-fence,
  /// cooperative step-down -- fired while this node's 409
  /// deferral was pending: the deferral's question was answered or its
  /// belief episode ended, so a LATER exhaustion from the same latch
  /// is stale (the production "two consecutive renew 409s" warn on
  /// 409s that were NOT consecutive). Reset by the conflict action (a
  /// fresh deferral starts unresolved). Frozen all-false at
  /// HOLDER_EVIDENCE_LOSE = false.
  var deferralResolvedV: Node -> bool
  /// History latch (writer: conflict's exhausted branch): a believing
  /// 409 ran the exhausted-deferral lose edge although a resolving
  /// action had fired since the deferral was set -- merged_bug_002's
  /// spurious failover. The fix scopes the latch to the belief episode
  /// (conflictDeferredV cleared by EVERY resolving completed read --
  /// fetchObservesOwnCommit, actFailedHeldReadObserved (bug_002: the
  /// frozen-content/empty-ledger holder=self read), loseObserved --
  /// plus selfFence and serveStepDown), making this latch
  /// unreachable; its twin invariant noStaleExhaustedLose is the
  /// regression pin.
  var staleExhaustedLose: bool
  /// Per-acquire tenure-instance counter (production
  /// LeaderState::acquire_instance): bumped on every acquire edge,
  /// never reused; reset by crash (the Arc dies with the process).
  /// Frozen at MAX_STEP_DOWNS = 0.
  var acquireInstV: Node -> int
  /// Pending cooperative step-down request (production
  /// step_down_for): the stamp the serve compares (count or instance
  /// per STEP_DOWN_COUNT_KEYED) plus the TRUE filing instance (the
  /// latch's ground truth). Frozen at MAX_STEP_DOWNS = 0.
  var stepDownReqV: Node -> StepDownState
  /// Step-down requests filed so far (0..MAX_STEP_DOWNS).
  var stepDownsV: int
  /// History latch: a step-down was SERVED by a tenure instance that
  /// did not file it (the merged_bug_128 stale demotion). Judged on
  /// the true filing instance, independent of the stamp keying.
  var staleStepDownServed: bool

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
  def obsKeyOf(n: Node): int = match obs.get(n) { | NoObs => -1 | Observed(o) => o.key }
  def snapContentOf(n: Node): int = match snap.get(n) { | NoSnap => -1 | Snap(l) => l.content }
  /// The observation/evidence identity key (`sched.lease.k8s-lease` /
  /// `sched.lease.cancelled-write`): the holder-authored content in
  /// production; the raw rv only under the lease-180-rv-keyed twin.
  def leaseKey(l: LeaseRec): int = if (OBS_KEY_RV) l.rv else l.content
  def snapKeyOf(n: Node): int = if (OBS_KEY_RV) snapRvOf(n) else snapContentOf(n)
  def obsSinceOf(n: Node): int = match obs.get(n) { | NoObs => -1 | Observed(o) => o.since }
  def hasPending(n: Node): bool = pending.get(n) != NoPending
  def anchorOf(n: Node): int = match pending.get(n) { | NoPending => -1 | PendingRead(p) => p.anchor }
  def unconfAnchorOf(n: Node): int = match unconfirmed.get(n) { | NoUnconf => -1 | Unconf(u) => u.anchor }
  /// Own-commit evidence is observable for n: a transmitted write is
  /// in doubt, the apiserver still (or again) names n holder, and the
  /// rv has MOVED past the evidence cursor -- some write of ours
  /// committed since the last completed fetch. Identically false at
  /// MAX_DROPS = 0 (the ledger is unmintable).
  def evidenceEnabled(n: Node): bool = and {
    unconfirmed.get(n) != NoUnconf,
    lease.holder == Held(n),
    leaseKey(lease) != lastFetchedKey.get(n),
  }
  /// The blindHolderFor ghost's single transition rule, applied in
  /// every action's frame with that action's POST values (the
  /// wrongSince discipline of leaderMarks.qnt -- no enumerated writer
  /// list): while some alive non-believer is the lease's holder with
  /// observable own-commit evidence, a tick increments and any other
  /// action holds; otherwise the ghost is 0. Frozen at MAX_DROPS = 0
  /// (byte-identical pre-existing regimes).
  def bhNext(lz: LeaseRec, ld: Node -> bool, al: Node -> bool, uc: Node -> UnconfState, cf: Node -> int, cur: int, isTick: bool): int =
    if (MAX_DROPS == 0) 0
    else if (NODES.exists(m => and {
      al.get(m),
      not(ld.get(m)),
      lz.holder == Held(m),
      uc.get(m) != NoUnconf,
      (if (OBS_KEY_RV) lz.rv else lz.content) != cf.get(m),
    }))
      (if (isTick) cur + 1 else cur)
    else 0

  /// held tracking, gated on the shutdown fault so the four existing
  /// regimes are STATE-SPACE-IDENTICAL at MAX_SHUTDOWNS = 0 (held stays
  /// at its init value; a constant variable multiplies no states).
  def trackHeld(cur: Node -> bool, n: Node, v: bool): Node -> bool =
    if (MAX_SHUTDOWNS > 0) cur.put(n, v) else cur

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
    drops <= MAX_DROPS,
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

  /// The graceful-handover invariant (sched.lease.graceful-release+2):
  /// no node ever exits while the apiserver still names it holder
  /// without running the release. With the hold-gated release this is
  /// an invariant (¬held ∧ holder == us is unreachable: the hold
  /// clears only on completed rounds that observed someone else, on
  /// crash, or on a previous exit); with the belief-gated release the
  /// fence-then-SIGTERM trace violates it — the lease-387-belief-gate
  /// calibration pins that falsification.
  val gracefulHandover = not(releaseSkippedWhileHolder)

  /// Witness (expect-violation): the suspend actually straddles an
  /// in-flight renew in the suspend regime — a read completing more
  /// than FENCE_AFTER past its anchor is reachable. Guards the suspend
  /// regime's boundedDualLeadership verdict against vacuity.
  val noSuspendStraddle = not(suspendStraddled)

  /// Witness (expect-violation): the fence-then-exit shape — a node
  /// exiting not-believing while the apiserver still names it holder —
  /// is reachable in the shutdown regime. Guards gracefulHandover
  /// against vacuity (a release gate nobody reaches proves nothing).
  val noFenceThenExit = not(fenceThenExit)

  /// The dropped-write window bound (merged_bug_303): whenever the
  /// lease's holder has stopped believing while own-commit evidence
  /// of its transmitted write is OBSERVABLE, that state is younger
  /// than BLIND_HOLDER_BOUND global ticks -- the evidence-forcing
  /// tick cap obliges the fetch within one poll of the eldest doubt,
  /// so a fenced holder re-believes instead of livelocking. The
  /// lease-303-blind-timeout calibration twin removes the evidence
  /// fetch (and its forcing cap) and the window stretches to the
  /// trace horizon -- the leaderless livelock, one cycle of it:
  /// committed-but-cancelled PUTs re-anchor every standby's staleness
  /// clock while the blind ex-holder cannot re-anchor its own belief.
  val blindHolderBounded = blindHolderFor <= BLIND_HOLDER_BOUND

  /// merged_bug_180: own-commit evidence is never fabricated from
  /// foreign rv churn — whenever the evidence action consumes the
  /// ledger, the holder-authored CONTENT actually moved past the
  /// node's last completed fetch (i.e. a write of OURS committed).
  /// Holds in the foreignRv regime exactly because the evidence
  /// cursor keys on content; the lease-180-rv-keyed falsify twin
  /// restores the pre-fix rv keying and violates this.
  val noFabricatedEvidence = not(evidenceFabricated)

  /// Witness (expect-violation in the foreignRv regime): own-commit
  /// evidence is actually consumed in a trace where foreign bumps are
  /// enabled — the noFabricatedEvidence verdict there is about a
  /// space where both the evidence machinery and the fault fire.
  val noEvidenceUnderForeignBumps = not(evidenceConsumed)

  /// sched.lease.holder-evidenced-lose (bughunt-4 signed Q3): a
  /// believing node's completed-round lose edge carries holder
  /// evidence -- a read naming another holder (loseObserved) or an
  /// exhausted one-round 409 deferral. The blind first-409 lose is
  /// writable only by the CONFLICT_IMMEDIATE_LOSE twin toggle; in the
  /// live world this latch is unreachable and the invariant holds.
  /// Falsify twin: lease-085-blind-conflict-lose.
  val loseRequiresHolderEvidence = not(blindConflictLose)
  /// Reachability witness for the deferral plane (expect [violation]
  /// in the holder-evidence regime): a believing 409 actually
  /// deferred at least once -- the non-vacuity check for
  /// loseRequiresHolderEvidence's world.
  val noDeferredConflict = NODES.forall(n => not(conflictDeferredV.get(n)))
  /// merged_bug_002 twin: the exhausted-deferral lose edge fires only
  /// when NOTHING resolved the deferral since it was set -- the 409s
  /// were genuinely consecutive. Pre-fix (the resolving actions kept
  /// the latch: own-commit fetch, self-fence, step-down) this VIOLATES
  /// in the holder-evidence regime -- the production spurious-failover
  /// trace; green once the clears scope the latch to the belief
  /// episode. Falsify evidence recorded in the fixing commit.
  val noStaleExhaustedLose = not(staleExhaustedLose)

  /// sched.recovery.step-down instance law (merged_bug_128): a
  /// step-down is served only by the tenure instance that filed it.
  /// The latch is judged on the TRUE filing instance, independent of
  /// the stamp keying, so the COUNT_KEYED twin (transition-count
  /// stamp + no acquire-edge clear, the pre-fix production code)
  /// falsifies it through the fence -> same-epoch re-acquire -> stale
  /// serve schedule. Falsify twin: lease-128-count-keyed-stepdown.
  val noStaleStepDownServed = not(staleStepDownServed)
  /// Reachability witness for the step-down plane (expect [violation]
  /// in the step-down regime): a request was actually filed.
  val noStepDownRequested = stepDownsV == 0

  /// Witness (expect-violation in the droppedWrite regime): a
  /// transmitted write actually drops -- the blindHolderBounded and
  /// neverDual verdicts there are about a space where the fault
  /// bites.
  val noDroppedCommit = drops == 0

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

  /// Violated when steal's own-row-tie retain branch has actually fired
  /// after a Lease deletion: the post-deletion same-epoch re-acquire that
  /// fetch-max-seed / generation-claim mandate (retain the entry
  /// generation when the claims-ledger row at the floor is this holder's
  /// own) is explored, not merely expressible. stealRetained is set only
  /// inside that branch of steal, so unlike a state-shape predicate this
  /// cannot be satisfied by an ordinary bump-and-claim acquisition.
  /// Checked against the deletion regime, where the divergence between
  /// the lease-derived and ledger-derived epoch sources arises.
  val noRetainAfterDeletion = not(deletes > 0 and stealRetained)

  /// Violated when a steal's claim target had to exceed a tied foreign
  /// floor after a Lease deletion: the surviving floor (owned by the
  /// deletion victim) ties the stealer's restarted entry and must be
  /// exceeded -- the bump that keeps staleLeaderHasStaleGeneration true
  /// while the victim still believes. stealBumpedForeignTie is set only
  /// inside that branch of steal, so unlike a state-shape predicate this
  /// cannot be satisfied by an ordinary bump-and-claim acquisition.
  /// Checked against the deletion regime by
  /// quint-leader-election-witness-deletion-foreign-tie (nix/quint.nix);
  /// the other regimes cannot reach the branch (without a deletion the
  /// lease's transition count never regresses, so a stealer's entry
  /// strictly exceeds every recorded floor and a renew-edge tie is
  /// always the holder's own row).
  val noForeignTieBumpAfterDeletion = not(deletes > 0 and stealBumpedForeignTie)

  /// Violated when a renew-edge re-acquire after a Lease deletion had to
  /// bump to genHW + 1 because the surviving claims-ledger floor exceeds
  /// what the recreated lease's transition count can vouch for (seedFor's
  /// first arm; renewLease case 3) -- the restore that keeps
  /// staleLeaderHasStaleGeneration true where the entry half alone would
  /// under-restore a recovered holder to the deletion victim's
  /// generation. renewBumpedAboveEntry is set only inside renewLease's
  /// edge branch on that condition, so unlike a state-shape predicate
  /// this cannot be satisfied by an ordinary bump-and-claim acquisition.
  /// Checked against the deletion regime by
  /// quint-leader-election-witness-deletion-floor-bump (nix/quint.nix);
  /// the other regimes cannot reach the arm (without a deletion the
  /// lease's transition count never regresses, so the floor never
  /// exceeds the entry generation any holder computes).
  val noFloorBumpAfterDeletion = not(deletes > 0 and renewBumpedAboveEntry)

  /// Violated when a write-ahead claim INSERT has failed: proves the
  /// PgFaults regime actually explores the proceed-on-failure path
  /// (claimFails) its "each PG fault alone is survivable" verdict is
  /// about. claimFails is reachable only inside steal's / renewLease's
  /// edge-branch claim disjunction, so a violation is always an
  /// acquisition whose claim failed. Deliberately fault-firing only:
  /// pinning a SUCCESSOR acquisition after the failed claim would need
  /// a history variable (nothing the failure zeroes can order the
  /// conjunction, unlike the restore witness below), so that boundary
  /// stays with the conjunction procedure in the PgFaults module
  /// header. Checked against the PgFaults regime only -- every other
  /// regime disables the fault (MAX_CLAIM_FAILURES = 0), so the
  /// predicate holds there by construction.
  val noClaimFailure = claimFailures == 0

  /// Violated when a PG point-in-time restore has occurred AND a later
  /// claim INSERT succeeded: pgRestore zeroes genHW, and only claimOk
  /// on an acquire edge (a steal or an edge renew) raises it again --
  /// the steady-state renew offers genHW itself, a no-op -- so the
  /// conjunction pins "restore, then a successful re-claim", the
  /// post-fault recovery the regime's survivability verdict is about
  /// (the same construction as noReacquisitionAfterDeletion above).
  /// Its violation also proves pgRestore itself fired. Checked against
  /// the PgFaults regime only -- every other regime disables the fault
  /// (MAX_RESTORES = 0).
  val noReclaimAfterRestore = not(restores > 0 and genHW > 0)

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
    (alive.get(n) and leading.get(n) and pending.get(n) == NoPending) implies
      (clocks.get(n) - fence.get(n) <= FENCE_AFTER + RENEW))

  /// loopInterval's suspend-regime form: downstream of a split renew
  /// the post-read believer legitimately exceeds the tick-cap bound for
  /// the resume-to-first-tick gap (the read stamps the OLD anchor; the
  /// suspended interval already passed) — the production residual
  /// clock.rs documents. The tick cap still blocks any FURTHER clock
  /// advance for that node (TLC explores no successor that ticks it),
  /// so the gap admits no additional time; the disjunct only excuses
  /// states a split renew can produce (suspends > 0), keeping the full
  /// tripwire on every pre-fault prefix — and the four legacy regimes
  /// (MAX_SUSPENDS = 0) keep the exact loopInterval.
  val loopIntervalResumeBounded = NODES.forall(n =>
    (alive.get(n) and leading.get(n) and pending.get(n) == NoPending) implies
      or {
        clocks.get(n) - fence.get(n) <= FENCE_AFTER + RENEW,
        suspends > 0,
      })

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
        // (5) mid-deferral (HOLDER_EVIDENCE_LOSE, bughunt-4 signed
        // Q3): a believing 409 deferred one round -- the deferral IS
        // the armed discovery mechanism. The deferred bounce does NOT
        // re-stamp the fence (see conflict), so the deferred believer
        // is still running down its PRE-409 fence budget: belief ends
        // at the last non-deferred stamp + FENCE_AFTER -- the same
        // deadline it had before the bounce -- unless a completed
        // round resolves it sooner (loseObserved / a second 409
        // exhausting it / an own-commit rv-mover, which also resets
        // the thief's steal clock). The deferral changes WHICH edge
        // runs, never how long belief may last; the disjunct exists
        // because the deferred believer may hold no snapshot (the
        // bounce spent it) while disjunct (1)'s near-deadline window
        // has not yet opened. Unreachable (frozen all-false) in every
        // regime with the gate off.
        conflictDeferredV.get(stale),
      }),
    }))

  /// The bridge to sched.lease.generation-fence+2. When the dual-belief
  /// window is open, the executor's generation fence has to be able to
  /// tell the replicas apart -- the fresh leader's generation must be
  /// strictly greater than the stale believer's. The production
  /// mechanism is on_acquire(transitions)'s fetch_max(leaseTransitions
  /// + 1) plus the write-ahead claim (sched.lease.generation-claim); the
  /// model encodes both in steal: gen[n]' = seedFor over the entry
  /// max(gen[n], lease.gen+2) -- exceeding the PG floor unless the
  /// ledger row at it is the stealer's own -- and genHW' advancing to
  /// that value whenever it exceeds the floor.
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
  ///       (lease.gen+2 in steal's entry) -- the apiserver bumps
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
  ///     keeps acting until the next real acquisition seeds at or above
  ///     the floor -- a foreign acquirer exceeds both; the higher
  ///     believer itself re-acquiring retains its own epoch, preserving
  ///     the same ordering. Distinctness is not a weakening that can hide
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
    // pending == NoPending: a loop parked on an in-flight read runs no
    // fence checks — the loop-discipline cap does not apply to it (the
    // honest statement of bug_096; vacuous at MAX_SUSPENDS = 0).
    (alive.get(n) and leading.get(n) and pending.get(n) == NoPending) implies
      ((clocks.get(n) + 1) - fence.get(n) <= FENCE_AFTER + RENEW),
    NODES.forall(m => (clocks.get(n) + 1) - clocks.get(m) <= MAX_SKEW),
    // The evidence-forcing cap (merged_bug_303): the loop polls every
    // RENEW and the fetch phase fits the mid-band budget, so a node
    // with own-commit evidence AVAILABLE cannot tick past the poll
    // that would observe it -- the candidate-side twin of the leading
    // cap above, anchored at the ledger's eldest doubt. Vacuous at
    // MAX_DROPS = 0.
    (alive.get(n) and pending.get(n) == NoPending and evidenceEnabled(n)) implies
      ((clocks.get(n) + 1) - unconfAnchorOf(n) <= FENCE_AFTER + RENEW),
    clocks' = clocks.put(n, clocks.get(n) + 1),
    lease' = lease, alive' = alive, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, true),
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
  /// clock. Production stamps the blind-window clock at exactly two
  /// sites -- run_lease_loop's loop init (before the node has ever
  /// led) and its `Ok(Ok(result))` arm after a COMPLETE
  /// try_acquire_or_renew() round-trip -- and the stamped VALUE is a
  /// RenewAnchor minted before the attempt's await, so the production
  /// anchor is at or BEFORE the apiserver commit this model anchors
  /// fence[n] at (anchor <= send <= commit; stamping a response-time
  /// reading has no API). Every path through that
  /// function which
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
    pending.get(n) == NoPending,
    // A GET that would observe own-commit evidence IS the evidence
    // fetch (fetchObservesOwnCommit) -- production evaluates the
    // evidence check inside every completed fetch before refreshing
    // the cursor, so a bare GET that ignores pending evidence does
    // not exist. Identically true at MAX_DROPS = 0.
    not(evidenceEnabled(n)),
    snap' = snap.put(n, Snap(lease)),
    obs' = obs.put(n,
      if (lease.holder == NoHolder) NoObs
      else if (lease.holder == Held(n)) obs.get(n)
      else if (hasObs(n) and obsKeyOf(n) == leaseKey(lease)) obs.get(n)
      else Observed({ key: leaseKey(lease), since: clocks.get(n) })),
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    fence' = fence, gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    // The hold clears when the GET observes a holder that is not us
    // (the model point where production's Standby resolution lands —
    // on_observed(false)); observing our own holdership keeps it.
    pending' = pending,
    held' = trackHeld(held, n, if (lease.holder == Held(n)) held.get(n) else false),
    exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // A GET observing a FOREIGN holder (or none) is the model point
    // where production's Standby resolution lands, and a COMPLETED
    // round answers the unconfirmed question wholesale (the fresh
    // stamp supersedes the ledger's older anchor) -- the
    // run_lease_loop Completed arm. Observing OURSELVES is only the
    // fetch half of a round still in flight: ledger and cursor wait
    // for the round's resolution. Value-identical at MAX_DROPS = 0.
    unconfirmed' =
      if (lease.holder == Held(n)) unconfirmed
      else unconfirmed.put(n, NoUnconf),
    // Cursor per the same arm (404/empty resets the baseline --
    // production's create path); gated for byte-identical
    // pre-existing regimes.
    lastFetchedKey' =
      if (MAX_DROPS == 0) lastFetchedKey
      else if (lease.holder == Held(n)) lastFetchedKey
      else if (lease.holder == NoHolder) lastFetchedKey.put(n, -1)
      else lastFetchedKey.put(n, leaseKey(lease)),
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    // Ghost content cursor mirrors the key cursor's branches (frozen
    // without foreign writers — content never diverges from rv there).
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else if (lease.holder == Held(n)) lastFetchedContent
      else if (lease.holder == NoHolder) lastFetchedContent.put(n, -1)
      else lastFetchedContent.put(n, lease.content),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    // The blind condition is invariant under a bare GET: the entry
    // only clears when the holder is foreign (that node's blind term
    // was already false), and no other coordinate moves.
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
  }

  /// The write-ahead claim INSERT succeeds: the PG floor advances to the
  /// claimed generation and the ledger row's owner is recorded. fetch_max
  /// in shape: a `claimed` at or below the current floor leaves both the
  /// floor and its owner unchanged -- the retain path (seedFor's own-row
  /// tie) and the steady-state renew (claimed = genHW) are exactly such
  /// provable no-ops, mirroring the implementation's "own row already at
  /// the target, nothing to INSERT" arm.
  ///
  /// A NAMED action (not an anonymous `all` block inside the claim
  /// disjunction) for the same reason every disjunct of `step` is named:
  /// quint's `--mbt` action tracker records the INNERMOST `any`
  /// disjunct's name, so an anonymous block here makes the whole
  /// steal/renewLease step anonymous in generated traces and undrivable
  /// by the model-based tests (rio-lease/src/mbt_tests.rs). The
  /// flattening discipline is recursive: no anonymous `any` disjunct
  /// anywhere reachable from `step`.
  action claimOk(owner: Holder, claimed: int): bool = all {
    genHW' = if (claimed > genHW) claimed else genHW,
    genHWHolder' = if (claimed > genHW) owner else genHWHolder,
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
    genHWHolder' = genHWHolder,
    claimFailures' = claimFailures + 1,
  }

  /// The implementation's claim-target rule (sched.recovery.fetch-max-seed /
  /// sched.lease.generation-claim): exceed every floor generation the
  /// claims ledger does not prove is this holder's own current epoch --
  /// retain the entry generation only when the floor ties it AND the
  /// ledger row at that floor is our own (own_claim_at_our_gen in
  /// handle_leader_acquired). A floor above the entry is someone else's
  /// (or our own pre-crash) higher claim and must be exceeded; a tied
  /// floor under a foreign row likewise; a floor below the entry cannot
  /// vouch for anything higher than what the lease already gave us.
  def seedFor(n: Node, entry: int): int =
    if (genHW > entry) genHW + 1
    else if (genHW == entry and genHWHolder != Held(n)) genHW + 1
    else entry

  /// The decide()->Steal path: empty-holder OR stale-observed (the
  /// deposed-leader path). gen[n] and genHW are set per
  /// on_acquire(transitions) + seed_generation_from() +
  /// claim_generation() collapsed: all three run before
  /// recovery_complete=true gates dispatch, so the model treats
  /// acquire/seed/claim as one atomic step.
  /// r[sched.recovery.fetch-max-seed] (prose reference; the tracey
  /// marker lives at the check wiring point in nix/quint.nix).
  ///
  /// The generation derives from the lease's POST-BUMP transition count:
  /// the PUT writes lease.gen+1 (= leaseTransitions after the holder
  /// change), and on_acquire() fetch_max'es (lease.gen+1)+1 = lease.gen+2
  /// into the atomic (the apiserver bumps the count atomically with the
  /// holder change inside the rv-guarded PUT, so two distinct holders can
  /// never derive the same value from it). The PG floor contributes
  /// genHW+1 via seedFor unless the ledger row at the floor is this
  /// holder's own (seed_generation_from plus the claims-ledger ownership
  /// check). Both are fetch_max against the
  /// atomic's current value gen[n] -- nothing unconditionally increments.
  ///
  /// genHW' is the write-ahead claim (sched.lease.generation-claim): the
  /// generation is durably recorded in PG's claims ledger before dispatch
  /// is ungated, so the floor advances at acquisition time. There is no
  /// acquire-to-persist window for a deposed leader to die inside -- the
  /// window the pre-fix counterexample exploited (see the FIXED note at
  /// staleLeaderHasStaleGeneration).
  ///
  /// Why the collapse stays sound against the real, non-atomic
  /// implementation: a holder change the local lease loop OBSERVED is
  /// caught by the actor's recovery discard gate (the generation +
  /// recorded acquire-transitions + is_leader re-check in
  /// handle_leader_acquired), which keeps recovery_complete false until
  /// a recovery that observed the post-change state completes -- even
  /// when the PG-seeded generation makes on_acquire's fetch_max a
  /// no-op. A holder change the loop has NOT yet observed moves none of
  /// those signals at the time; the next still-leading round that
  /// observes the moved transition count re-records it, clears
  /// recovery_complete, and re-fires the acquire hook
  /// (sched.lease.rebound), so a foreign tenure that began and ended
  /// inside the observation gap re-runs recovery unless its count lands
  /// back exactly on the recorded value. For the recovery still in
  /// flight, a claim target the durable PG floor cannot vouch for --
  /// above the entry generation, or a retained entry generation more
  /// than one above the floor -- is additionally not seeded without a
  /// post-claim round-trip that ended with this replica as the Lease
  /// holder (sched.recovery.bump-confirm), so a deposed-but-unaware
  /// believer cannot COMPLETE recovery above a live successor when the
  /// durable history exposes the gap. The acquire-to-claim crash window
  /// (a leader that consumed a transition count but died before its
  /// claim INSERT) is real in the implementation and unrepresentable
  /// here precisely because this step collapses acquire/seed/claim;
  /// the non-vouched-retain confirmation is what covers it. Residuals,
  /// all priced where they are accepted: the count-coincidence ABA
  /// documented at the gate's entry-snapshot comment in
  /// handle_leader_acquired (an edge-ful re-steal, or a rebound, whose
  /// observed count lands back on the recorded value -- it adds no
  /// dispatch interleaving beyond what that acceptance already covers);
  /// the claim-failure conjunction documented with the PgFaults regime;
  /// and the adjacent-floor race, where the successor's claim at the
  /// believer's entry generation minus one lands before the believer's
  /// floor read and the floor looks contiguous.
  action steal(n: Node): bool = {
    // entry = on_acquire's fetch_max(transitions + 1) with the steal
    // PUT's post-bump count (lease.gen + 2), against the Arc's current
    // value gen[n] -- the recovery-entry generation the claim target is
    // baselined on.
    val entry = if (gen.get(n) > lease.gen + 2) gen.get(n) else lease.gen + 2
    val seeded = seedFor(n, entry)
    all {
      alive.get(n),
      pending.get(n) == NoPending,
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
          obsKeyOf(n) == snapKeyOf(n),
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
      lease' = { holder: Held(n), rv: snapRvOf(n) + 1, content: lease.content + 1, gen: lease.gen + 1 },
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
      // The retain-event discriminator for the noRetainAfterDeletion
      // witness: records that this acquisition kept its entry generation
      // because the ledger row at the floor is its own (evaluated on the
      // same pre-state values seedFor reads). The parentheses around the
      // whole right-hand side are load-bearing: `or` binds looser than
      // `=`, so without them this conjunct would parse as a disjunction
      // containing an assignment instead of an assignment of a
      // disjunction, leaving the variable unassigned on one branch.
      stealRetained' = (stealRetained or
        (genHW == entry and genHWHolder == Held(n))),
      // The bump-event discriminator for the noForeignTieBumpAfterDeletion
      // witness: records that this acquisition had to exceed a tied
      // FOREIGN floor (same pre-state values seedFor reads, same
      // load-bearing parentheses as the conjunct above; the not-equal
      // form deliberately covers the ownerless tie too -- "not our row
      // => bump" is the claim-target semantics).
      stealBumpedForeignTie' = (stealBumpedForeignTie or
        (genHW == entry and genHWHolder != Held(n))),
      renewBumpedAboveEntry' = renewBumpedAboveEntry,
      pending' = pending, held' = trackHeld(held, n, true), exited' = exited,
      suspends' = suspends, shutdowns' = shutdowns,
      releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
      suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
      // A steal is a completed round: the unconfirmed question is
      // answered wholesale; the cursor takes the post-PUT rv (gated
      // for byte-identical pre-existing regimes).
      unconfirmed' = unconfirmed.put(n, NoUnconf),
      lastFetchedKey' =
        if (MAX_DROPS == 0) lastFetchedKey
        else lastFetchedKey.put(n, snapKeyOf(n) + 1),
      drops' = drops,
      foreignBumps' = foreignBumps,
      // Acquire edge: a new tenure instance begins -- the per-acquire
      // counter bumps and a pending step-down clears (production
      // on_acquire; the COUNT_KEYED twin restores the pre-fix world
      // with no clear). A resolved acquisition also clears a pending
      // 409 deferral. All gated so disabled planes stay frozen.
      conflictDeferredV' =
        if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
        else conflictDeferredV.put(n, false),
      deferralResolvedV' = deferralResolvedV,
      staleExhaustedLose' = staleExhaustedLose,
      blindConflictLose' = blindConflictLose,
      acquireInstV' =
        if (MAX_STEP_DOWNS == 0) acquireInstV
        else acquireInstV.put(n, acquireInstV.get(n) + 1),
      stepDownReqV' =
        if (MAX_STEP_DOWNS == 0 or STEP_DOWN_COUNT_KEYED) stepDownReqV
        else stepDownReqV.put(n, NoStepDown),
      stepDownsV' = stepDownsV,
      staleStepDownServed' = staleStepDownServed,
      lastFetchedContent' =
        if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
        else lastFetchedContent.put(n, snapContentOf(n) + 1),
      evidenceFabricated' = evidenceFabricated,
      evidenceConsumed' = evidenceConsumed,
      blindHolderFor' = 0,  // the thief is the holder AND a believer post-state
      // The claim disjunction: the INSERT into the claims ledger either
      // lands (claimOk -- the floor advances when seeded exceeds it; on
      // seedFor's own-row-tie retain path seeded equals genHW, so claimOk
      // is a provable no-op and claimFails is disabled by its
      // `claimed > genHW` guard, exactly the implementation's "own row
      // already at the target, nothing to INSERT" arm) or errors
      // (claimFails -- the ClaimFails fault). Both disjuncts are NAMED
      // actions; see claimOk's doc for why an anonymous block here
      // breaks the model-based tests.
      any {
        claimOk(Held(n), seeded),
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
  ///            (steal's entry uses lease.gen+2 because the steal PUT
  ///            bumps the count first; a renew does not)
  ///   seeded = seedFor(n, entry), the shared claim-target rule: a floor
  ///            ABOVE the entry must be exceeded (someone -- possibly our
  ///            own pre-crash incarnation -- claimed higher); a floor
  ///            tying it is retained only when the ledger row at it is
  ///            our own (genHWHolder), else exceeded. The foreign-tie
  ///            bump branch belongs to the shared seedFor rule but never
  ///            fires via renewLease in any regime: a renewing node holds
  ///            the lease, and a floor tying its entry is its own claim
  ///            row (the retain arm). Its reachable instances are
  ///            steal-side and deletion-only -- after a deleteLease the
  ///            surviving floor (owned by the deletion victim) ties a
  ///            foreign stealer's restarted entry and must be exceeded;
  ///            that bump is what keeps staleLeaderHasStaleGeneration
  ///            true while the victim still believes, and the
  ///            quint-leader-election-witness-deletion-foreign-tie check
  ///            (nix/quint.nix) pins that the deletion regime actually
  ///            explores it. Without a deletion the lease's transition
  ///            count never regresses, so a stealer's entry strictly
  ///            exceeds every recorded floor and a tied floor at a renew
  ///            edge is always the holder's own row -- base, asymmetric,
  ///            and pg-faults cannot reach the branch (claim failures
  ///            only lower the floor; they cannot manufacture a foreign
  ///            tie).
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
  /// The three cases, the first two pinned by checker runs and the third
  /// by the noFloorBumpAfterDeletion witness check (nix/quint.nix):
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
    val seeded = seedFor(n, entry)
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
      pending.get(n) == NoPending,
      hasSnap(n),
      snapHolderOf(n) == Held(n),
      casOk(n),
      // The generation ceiling only constrains the edge branch -- a
      // steady-state renew computes no new generation and must not be
      // disabled at the ceiling.
      isEdge implies (seeded <= MAX_GEN),
      lease' = { ...lease, rv: snapRvOf(n) + 1, content: lease.content + 1 },
      leading' = leading.put(n, true),
      gen' = gen.put(n, if (isEdge) seeded else gen.get(n)),
      casRace' = casRaceAfterPut(n),
      fence' = fence.put(n, clocks.get(n)),
      snap' = snap.put(n, NoSnap),
      clocks' = clocks, alive' = alive, obs' = obs,
      acquiredAt' = acquiredAt, deletes' = deletes,
      delVictims' = delVictims, restores' = restores,
      stealRetained' = stealRetained,
      stealBumpedForeignTie' = stealBumpedForeignTie,
      // The bump-event discriminator for the noFloorBumpAfterDeletion
      // witness: records that this edge re-acquire's claim target had to
      // exceed a floor strictly ABOVE the entry generation (seedFor's
      // first arm -- renewLease's case 3). Evaluated on the same
      // pre-state values seedFor reads; the parentheses around the
      // right-hand side are load-bearing (`or` binds looser than `=`).
      renewBumpedAboveEntry' = (renewBumpedAboveEntry or
        (isEdge and genHW > entry)),
      pending' = pending, held' = trackHeld(held, n, true), exited' = exited,
      suspends' = suspends, shutdowns' = shutdowns,
      releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
      suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
      // A completed renew answers the unconfirmed question wholesale;
      // the cursor takes the post-PUT rv (gated for byte-identical
      // pre-existing regimes).
      unconfirmed' = unconfirmed.put(n, NoUnconf),
      lastFetchedKey' =
        if (MAX_DROPS == 0) lastFetchedKey
        else lastFetchedKey.put(n, snapKeyOf(n) + 1),
      drops' = drops,
      foreignBumps' = foreignBumps,
      // A completed Leading round resolves any pending 409 deferral;
      // on the acquire EDGE the tenure instance bumps and a pending
      // step-down clears (production on_acquire -- the steady-state
      // renew is not an edge and touches neither).
      conflictDeferredV' =
        if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
        else conflictDeferredV.put(n, false),
      deferralResolvedV' = deferralResolvedV,
      staleExhaustedLose' = staleExhaustedLose,
      blindConflictLose' = blindConflictLose,
      acquireInstV' =
        if (MAX_STEP_DOWNS == 0 or not(isEdge)) acquireInstV
        else acquireInstV.put(n, acquireInstV.get(n) + 1),
      stepDownReqV' =
        if (MAX_STEP_DOWNS == 0 or STEP_DOWN_COUNT_KEYED or not(isEdge)) stepDownReqV
        else stepDownReqV.put(n, NoStepDown),
      stepDownsV' = stepDownsV,
      staleStepDownServed' = staleStepDownServed,
      lastFetchedContent' =
        if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
        else lastFetchedContent.put(n, snapContentOf(n) + 1),
      evidenceFabricated' = evidenceFabricated,
      evidenceConsumed' = evidenceConsumed,
      blindHolderFor' = 0,  // the renewer is the holder AND a believer post-state
      // The same claim disjunction as steal's, restricted to the edge
      // branch via claimTarget -- a steady-state renew never claims, so
      // it cannot fail to claim. The retry-on-the-next-acquire-edge
      // behavior falls out: a failed claim leaves genHW < gen[n], and
      // the NEXT edge renew (or steal) recomputes seeded and offers the
      // ledger the claim again.
      any {
        claimOk(Held(n), claimTarget),
        claimFails(claimTarget),
      },
    }
  }

  /// The 409 path: the snapshot is stale. Stamps fence (the apiserver
  /// answered). If we were Leading this is an explicit lose -- someone
  /// stole between our GET and PUT. Production distinguishes
  /// 409-on-renew (lose) from 409-on-steal (never led); both end up
  /// not-leading, both reset fence -- the model collapses them.
  action conflict(n: Node): bool = {
    // The believing first-409 deferral (HOLDER_EVIDENCE_LOSE, signed
    // Q3): the 409 proves only rv movement -- the mover may be this
    // node's own dropped-then-committed write or a foreign bumpRv --
    // so a believing node's FIRST 409 keeps belief, the hold, and the
    // ledger, and owes its resolution to the next completed read
    // (loseObserved / renewLease / a second 409 exhausting it).
    val defers = and {
      HOLDER_EVIDENCE_LOSE,
      not(CONFLICT_IMMEDIATE_LOSE),
      leading.get(n),
      not(conflictDeferredV.get(n)),
    }
    // The pre-fix blind lose, restorable only by the twin toggle: a
    // believing FIRST 409 running the lose edge with neither holder
    // evidence nor an exhausted deferral. The latch is judged on the
    // deferral ground truth, independent of the toggle's behavior.
    val blindLose = and {
      HOLDER_EVIDENCE_LOSE,
      CONFLICT_IMMEDIATE_LOSE,
      leading.get(n),
      not(conflictDeferredV.get(n)),
    }
    // The exhausted-deferral lose edge: a believing 409 with the
    // deferral latch still set (production's ConflictResolution::
    // Exhausted). Judged stale -- merged_bug_002 -- when a resolving
    // action fired since the deferral was set (deferralResolvedV).
    val exhausts = and {
      HOLDER_EVIDENCE_LOSE,
      not(CONFLICT_IMMEDIATE_LOSE),
      leading.get(n),
      conflictDeferredV.get(n),
    }
    all {
      alive.get(n),
      pending.get(n) == NoPending,
      hasSnap(n),
      lease.rv != snapRvOf(n),
      // A DEFERRED 409 does NOT re-stamp the fence: the deferral runs
      // on the pre-409 blind budget. A stamped deferral would extend
      // belief with the lease content frozen -- decoupled from the
      // content writes that reset a standby's steal clock -- and this
      // regime's checker produces the dual-belief counterexample
      // (victim defers near its fence deadline, believes past the
      // standby's steal). Production mirrors this: the loop computes
      // will_defer BEFORE the blind stamp. Resolved 409s stamp as
      // before (belief ends or continues under a fresh content
      // write).
      fence' = if (defers) fence else fence.put(n, clocks.get(n)),
      leading' = if (defers) leading else leading.put(n, false),
      snap' = snap.put(n, NoSnap),
      clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
      gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
      acquiredAt' = acquiredAt,
      casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
      claimFailures' = claimFailures, restores' = restores,
      stealRetained' = stealRetained,
      stealBumpedForeignTie' = stealBumpedForeignTie,
      renewBumpedAboveEntry' = renewBumpedAboveEntry,
      // A resolved 409 is a completed round resolving not-leading:
      // the hold clears (production's on_observed_not_leading). A
      // DEFERRED 409 keeps the hold -- no transition happened.
      pending' = pending,
      held' = if (defers) held else trackHeld(held, n, false),
      exited' = exited,
      suspends' = suspends, shutdowns' = shutdowns,
      releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
      suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
      // Under the holder-evidence law a 409 NEVER answers the
      // unconfirmed question (the pre-move GET cannot say whether the
      // rv-mover was ours): the ledger survives every Conflict, which
      // is exactly what lets the next completed read consume it as
      // own-commit evidence. The legacy worlds (gate off, or the twin
      // restoring the pre-fix) clear it wholesale, as production did.
      unconfirmed' =
        if (HOLDER_EVIDENCE_LOSE and not(CONFLICT_IMMEDIATE_LOSE)) unconfirmed
        else unconfirmed.put(n, NoUnconf),
      lastFetchedKey' =
        if (MAX_DROPS == 0) lastFetchedKey
        else lastFetchedKey.put(n, snapKeyOf(n)),
      drops' = drops,
      foreignBumps' = foreignBumps,
      conflictDeferredV' =
        if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
        else conflictDeferredV.put(n, defers),
      blindConflictLose' = (blindConflictLose or blindLose),
      // A conflict either mints a FRESH deferral (nothing resolved it
      // yet) or consumes/clears the latch -- either way the node's
      // resolved-since ghost resets.
      deferralResolvedV' =
        if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
        else deferralResolvedV.put(n, false),
      // merged_bug_002's defect latch: exhaustion firing although a
      // resolving action answered the deferral in between.
      staleExhaustedLose' =
        (staleExhaustedLose or (exhausts and deferralResolvedV.get(n))),
      acquireInstV' = acquireInstV,
      stepDownReqV' = stepDownReqV,
      stepDownsV' = stepDownsV,
      staleStepDownServed' = staleStepDownServed,
      lastFetchedContent' =
        if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
        else lastFetchedContent.put(n, snapContentOf(n)),
      evidenceFabricated' = evidenceFabricated,
      evidenceConsumed' = evidenceConsumed,
      blindHolderFor' = bhNext(
        lease,
        if (defers) leading else leading.put(n, false),
        alive,
        if (HOLDER_EVIDENCE_LOSE and not(CONFLICT_IMMEDIATE_LOSE)) unconfirmed
        else unconfirmed.put(n, NoUnconf),
        lastFetchedKey,
        blindHolderFor,
        false
      ),
    }
  }

  /// The holder-evidenced lose (HOLDER_EVIDENCE_LOSE only; the gate
  /// keeps pre-existing regimes' transition relations unchanged): a
  /// believing node's completed read names ANOTHER holder -- the
  /// production Completed(Standby) resolution while believing, i.e.
  /// the lose edge WITH the evidence the signed Q3 law demands. Also
  /// the resolution path for a pending 409 deferral whose rv-mover
  /// was a genuine steal.
  action loseObserved(n: Node): bool = all {
    HOLDER_EVIDENCE_LOSE,
    alive.get(n),
    pending.get(n) == NoPending,
    hasSnap(n),
    leading.get(n),
    snapHolderOf(n) != Held(n),
    snapHolderOf(n) != NoHolder,
    fence' = fence.put(n, clocks.get(n)),
    leading' = leading.put(n, false),
    snap' = snap.put(n, NoSnap),
    clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
    gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = trackHeld(held, n, false), exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // A Standby resolution answers the ledger wholesale (production:
    // only the non-Conflict Completed arms clear it).
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' =
      if (MAX_DROPS == 0) lastFetchedKey
      else lastFetchedKey.put(n, snapKeyOf(n)),
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV.put(n, false),
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, snapContentOf(n)),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading.put(n, false), alive, unconfirmed.put(n, NoUnconf), lastFetchedKey, blindHolderFor, false),
  }


  /// File a cooperative step-down request (MAX_STEP_DOWNS > 0 only):
  /// the production actor's failed-recovery path
  /// (sched.recovery.step-down). The stamp is what the serve compares
  /// -- the per-acquire instance, or the generation-derived count
  /// under the COUNT_KEYED twin (gen is fetch_max-derived from the
  /// lease transition count, so a same-epoch re-acquire repeats it
  /// exactly as production acquired_transitions did). `inst` records
  /// the TRUE filing instance regardless: the latch's ground truth.
  action requestStepDown(n: Node): bool = all {
    MAX_STEP_DOWNS > 0,
    stepDownsV < MAX_STEP_DOWNS,
    alive.get(n),
    leading.get(n),
    stepDownReqV.get(n) == NoStepDown,
    stepDownReqV' = stepDownReqV.put(n, StepDownReq({
      stamp: if (STEP_DOWN_COUNT_KEYED) gen.get(n) else acquireInstV.get(n),
      inst: acquireInstV.get(n),
    })),
    stepDownsV' = stepDownsV + 1,
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops, foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = blindHolderFor,
  }

  /// Serve a pending step-down at a believing tick (MAX_STEP_DOWNS >
  /// 0 only): the production loop's holder-guarded release + full
  /// lose edge. The serve guard compares the request's STAMP against
  /// the current tenure identity (instance, or generation under the
  /// COUNT_KEYED twin); the staleStepDownServed latch is judged on
  /// the TRUE filing instance, independent of the keying -- it fires
  /// exactly when a tenure instance is demoted by a request it never
  /// filed (the merged_bug_128 failover).
  action serveStepDown(n: Node): bool = all {
    MAX_STEP_DOWNS > 0,
    alive.get(n),
    leading.get(n),
    lease.holder == Held(n),
    // Scope bound, same shape as every other lease writer: the
    // release is an rv (and content) bump.
    lease.rv < MAX_RV,
    match stepDownReqV.get(n) {
      | NoStepDown => false
      | StepDownReq(r) =>
        r.stamp == (if (STEP_DOWN_COUNT_KEYED) gen.get(n) else acquireInstV.get(n))
    },
    lease' = { ...lease, holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1 },
    leading' = leading.put(n, false),
    fence' = fence.put(n, clocks.get(n)),
    held' = trackHeld(held, n, false),
    stepDownReqV' = stepDownReqV.put(n, NoStepDown),
    staleStepDownServed' = (staleStepDownServed or
      match stepDownReqV.get(n) {
        | NoStepDown => false
        | StepDownReq(r) => r.inst != acquireInstV.get(n)
      }),
    clocks' = clocks, alive' = alive, snap' = snap, obs' = obs,
    gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops, foreignBumps' = foreignBumps,
    // A served step-down ends the belief episode (production routes
    // through on_observed_not_leading / on_self_fence, both of which
    // clear the episode latch -- merged_bug_002). Guard idiom keeps
    // gate-off regimes state-space-identical.
    conflictDeferredV' =
      if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
      else conflictDeferredV.put(n, false),
    // -- and the resolved-since ghost records the resolution.
    deferralResolvedV' =
      if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
      else deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownsV' = stepDownsV,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(
      { ...lease, holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1 },
      leading.put(n, false), alive, unconfirmed, lastFetchedKey,
      blindHolderFor, false
    ),
  }

  /// maybe_self_fence(): a Leading node that has not had a successful
  /// apiserver round-trip in > FENCE_AFTER per its OWN clock flips
  /// is_leader=false locally, without an apiserver write. The production
  /// loop checks this every RENEW_INTERVAL; the model lets it fire any
  /// time the deadline has passed (a superset of the production schedule
  /// -- sound for safety).
  action selfFence(n: Node): bool = all {
    alive.get(n),
    // A loop parked on an in-flight read runs no fence checks; the
    // first POST-READ tick is where the fence fires (vacuous at
    // MAX_SUSPENDS = 0). held is deliberately UNTOUCHED below — the
    // belief/hold split (bug_387): fencing is local.
    pending.get(n) == NoPending,
    leading.get(n),
    clocks.get(n) - fence.get(n) > FENCE_AFTER,
    leading' = leading.put(n, false),
    clocks' = clocks, lease' = lease, alive' = alive, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    // The self-fence ends the belief episode: production's
    // on_self_fence clears the episode latch (merged_bug_002) -- a
    // new belief episode starts clean. Guard idiom keeps gate-off
    // regimes state-space-identical.
    conflictDeferredV' =
      if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
      else conflictDeferredV.put(n, false),
    // -- and the resolved-since ghost records the resolution.
    deferralResolvedV' =
      if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
      else deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading.put(n, false), alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
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
    clocks' = clocks, lease' = lease, genHW' = genHW,
    genHWHolder' = genHWHolder, casRace' = casRace,
    deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    // The crash kills the in-flight read with the process (the PUT it
    // committed stays committed — exactly a real apiserver) and loses
    // the in-memory hold.
    pending' = pending.put(n, NoPending),
    held' = trackHeld(held, n, false),
    exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // The ledger and the cursor are in-memory: the crash loses both
    // (which is why evidence can never fire post-crash at the reset
    // generation -- the entry died with the incarnation).
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' = lastFetchedKey.put(n, -1),
    drops' = drops,
    foreignBumps' = foreignBumps,
    // In-memory plane state dies with the process: the deferral flag,
    // the per-acquire instance counter (the Arc), and any pending
    // step-down request reset. Value-identical no-ops when the planes
    // are disabled (the vars are frozen at their defaults).
    conflictDeferredV' = conflictDeferredV.put(n, false),
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV.put(n, 0),
    stepDownReqV' = stepDownReqV.put(n, NoStepDown),
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading.put(n, false), alive.put(n, false), unconfirmed.put(n, NoUnconf), lastFetchedKey, blindHolderFor, false),
  }

  /// Pod restart. The recovered process has no observation (its first
  /// decide() returns StartObserving and waits a full STEAL_AFTER before
  /// stealing) unless the lease still carries its identity (decide()
  /// returns Renew -- the recovered leader re-acquires its own lease
  /// without contention).
  action recover(n: Node): bool = all {
    not(alive.get(n)),
    // A graceful exit is terminal: SIGTERM'd processes do not restart
    // within the trace (the Deployment replaces the POD; a fresh
    // replica is a fresh node identity, out of scope at |NODES| = 2).
    not(exited.get(n)),
    alive' = alive.put(n, true),
    clocks' = clocks, lease' = lease, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive.put(n, true), unconfirmed, lastFetchedKey, blindHolderFor, false),
  }

  /// Suspend fault, send half: a steady-state renew whose PUT commits
  /// at the apiserver NOW while the response read is delayed (the loop
  /// parks on the await). The fence is UNCHANGED — the anchor is
  /// captured into the pending read, and only renewReadOk stamps it
  /// (the production RenewAnchor/BlindClock encoding). The rv bumps at
  /// send: a thief's staleness clock measures from this write, exactly
  /// like production (the apiserver committed; only the response is in
  /// flight). Only the current holder steady-renews (the acquire edge
  /// is not split: an acquiring node that suspends mid-acquire fails
  /// its round and is covered by the error arm — no stamp).
  action renewSendSteady(n: Node): bool = all {
    suspends < MAX_SUSPENDS,
    alive.get(n),
    leading.get(n),
    pending.get(n) == NoPending,
    hasSnap(n),
    snapHolderOf(n) == Held(n),
    casOk(n),
    lease' = { ...lease, rv: snapRvOf(n) + 1, content: lease.content + 1 },
    snap' = snap.put(n, NoSnap),
    pending' = pending.put(n, PendingRead({ anchor: clocks.get(n) })),
    suspends' = suspends + 1,
    casRace' = casRaceAfterPut(n),
    held' = trackHeld(held, n, true),
    fence' = fence, clocks' = clocks, alive' = alive, leading' = leading,
    obs' = obs, gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    exited' = exited, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    // The round is unresolved (the response is parked): ledger and
    // cursor wait for renewReadOk's resolution.
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = 0,  // the holder is a believer; no other node is named
  }

  /// Suspend fault, read half: the parked response arrives. THE FIX
  /// ENCODED: the fence restarts at the captured ANCHOR — never at the
  /// read instant (BlindClock has no API for that; the calibration
  /// override lease-096-response-anchor re-introduces it and must
  /// violate boundedDualLeadership). The straddle latch records a read
  /// completing more than FENCE_AFTER past its anchor — the suspended
  /// interval the response would have erased under response anchoring.
  action renewReadOk(n: Node): bool = all {
    alive.get(n),
    pending.get(n) != NoPending,
    fence' = fence.put(n, anchorOf(n)),
    pending' = pending.put(n, NoPending),
    // Parentheses load-bearing: `or` binds looser than `=` (the steal
    // action's discriminator comment is the precedent).
    suspendStraddled' = (suspendStraddled or
      (clocks.get(n) - anchorOf(n) > FENCE_AFTER)),
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    fenceThenExit' = fenceThenExit,
    // The parked read arriving IS the completed round: wholesale
    // answer. The cursor stays frozen -- the suspend and dropped-write
    // faults are never co-enabled in any wired regime, and the
    // pending record deliberately does not carry the PUT-time rv
    // (adding it would change the suspend regime's state space).
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed.put(n, NoUnconf), lastFetchedKey, blindHolderFor, false),
  }

  /// The dropped-write fault (merged_bug_303): a renew-shaped PUT
  /// whose apiserver write COMMITS while the renew composition fails
  /// after transmission -- the mid-band act deadline (the fetch phase
  /// fit its budget; the act's response never arrives). The loop is
  /// NOT parked (contrast renewSendSteady): the round resolves as an
  /// error, NO clock is stamped, and belief is UNTOUCHED -- decay is
  /// selfFence's job. The fetch half completed first, so the evidence
  /// cursor advances to the fetched rv; the transmitted write is
  /// recorded in the UnconfirmedPut ledger at the attempt anchor,
  /// OLDEST-kept (an unconsumed entry is never overwritten -- if
  /// several writes are in doubt the blind window must cover the
  /// eldest; production run_lease_loop's mint arm). Fires for
  /// believers AND fenced candidates alike -- a candidate whose lease
  /// still names it renews, and its re-acquire PUT drops the same
  /// way; that recurrence is the livelock's engine. Never enabled at
  /// MAX_DROPS = 0.
  action renewSendDropped(n: Node): bool = all {
    drops < MAX_DROPS,
    alive.get(n),
    pending.get(n) == NoPending,
    hasSnap(n),
    snapHolderOf(n) == Held(n),
    casOk(n),
    lease' = { ...lease, rv: snapRvOf(n) + 1, content: lease.content + 1 },
    snap' = snap.put(n, NoSnap),
    unconfirmed' = unconfirmed.put(n,
      if (unconfirmed.get(n) == NoUnconf) Unconf({ anchor: clocks.get(n) })
      else unconfirmed.get(n)),
    lastFetchedKey' = lastFetchedKey.put(n, snapKeyOf(n)),
    drops' = drops + 1,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, snapContentOf(n)),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    casRace' = casRaceAfterPut(n),
    held' = held,
    blindHolderFor' = bhNext(
      { ...lease, rv: snapRvOf(n) + 1, content: lease.content + 1 }, leading, alive,
      unconfirmed.put(n,
        if (unconfirmed.get(n) == NoUnconf) Unconf({ anchor: clocks.get(n) })
        else unconfirmed.get(n)),
      lastFetchedKey.put(n, snapKeyOf(n)),
      blindHolderFor, false),
    fence' = fence, clocks' = clocks, alive' = alive, leading' = leading,
    obs' = obs, gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
  }

  /// The dropped-write fault's DISCARDED flavor (bughunt3-wave S6b
  /// merged_bug_180): a renew-shaped PUT transmitted and abandoned
  /// like renewSendDropped — same client-side view, same ledger mint,
  /// same cursor advance — whose apiserver write NEVER applied (the
  /// connection died before the request was processed). "Cancelled is
  /// not discarded" means the client cannot distinguish the two: the
  /// model explores BOTH resolutions of every transmitted-abandoned
  /// write. The holder-authored content does NOT move here — which is
  /// exactly why rv-keyed evidence is laundering when a foreign bump
  /// moves the rv anyway (the lease-180-rv-keyed twin's engine).
  /// Gated on the foreign-bump axis (this flavor ships with it) so
  /// the pre-existing droppedWrite regime's state space is untouched
  /// — re-measured byte-identical at the introducing commit.
  action renewSendLost(n: Node): bool = all {
    MAX_FOREIGN_BUMPS > 0,
    drops < MAX_DROPS,
    alive.get(n),
    pending.get(n) == NoPending,
    hasSnap(n),
    snapHolderOf(n) == Held(n),
    casOk(n),
    lease' = lease,
    snap' = snap.put(n, NoSnap),
    unconfirmed' = unconfirmed.put(n,
      if (unconfirmed.get(n) == NoUnconf) Unconf({ anchor: clocks.get(n) })
      else unconfirmed.get(n)),
    lastFetchedKey' = lastFetchedKey.put(n, snapKeyOf(n)),
    drops' = drops + 1,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent.put(n, snapContentOf(n)),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    casRace' = casRace,
    held' = held,
    blindHolderFor' = bhNext(
      lease, leading, alive,
      unconfirmed.put(n,
        if (unconfirmed.get(n) == NoUnconf) Unconf({ anchor: clocks.get(n) })
        else unconfirmed.get(n)),
      lastFetchedKey.put(n, snapKeyOf(n)),
      blindHolderFor, false),
    fence' = fence, clocks' = clocks, alive' = alive, leading' = leading,
    obs' = obs, gen' = gen, genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
  }

  /// The own-commit evidence rule (merged_bug_303's fix): a COMPLETED
  /// fetch observing holder == us with the rv MOVED past the evidence
  /// cursor while a transmitted write is in doubt. Consumes the
  /// ledger and stamps the blind-window clock at the LEDGER's anchor
  /// -- anchor <= send <= commit, never the observing read's time:
  /// read-stamping is exactly the manufactured fence-creep the apiGet
  /// comment forbids, and stamping at the anchor keeps the fence and
  /// the thief's obs clock anchored to the same event (the last
  /// completed write), which is what the fence/steal separation
  /// argument requires. A frozen rv stamps NOTHING (no commit, no
  /// evidence; the window keeps aging and the fence fires on
  /// schedule -- the rv-frozen companion direction). If the believer
  /// self-fenced inside the blind window, belief re-enters through
  /// the ordinary acquire edge with the FETCHED transition count: the
  /// same-count same-epoch re-acquire -- the generation fetch_max is
  /// encoded verbatim and is a provable no-op here (the holder never
  /// changed, so the transition count never moved, and a crash wiped
  /// the ledger so a reset-gen incarnation can never reach this
  /// action). The bump-confirmation is deliberately NOT run --
  /// confirmation stays a completed-round property
  /// (sched.recovery.bump-confirm). Counts as a Leading observation
  /// for the hold. Never enabled at MAX_DROPS = 0.
  action fetchObservesOwnCommit(n: Node): bool = all {
    alive.get(n),
    pending.get(n) == NoPending,
    evidenceEnabled(n),
    fence' = fence.put(n, unconfAnchorOf(n)),
    // One action = one production loop pass (rio-lease lib.rs evidence
    // arm): the evidence stamp restores belief AND the fence still
    // arbitrates in the SAME pass — a ledger anchor already past
    // FENCE_AFTER re-fences before the pass ends, so belief never
    // outlives the discipline loopInterval states. Caught by TLC on
    // the foreign-rv regime: a LOST write's eldest-kept anchor consumed
    // via a LATER committing write's evidence re-believed 7 ticks stale.
    leading' = leading.put(n, clocks.get(n) - unconfAnchorOf(n) <= FENCE_AFTER),
    gen' = gen.put(n, if (gen.get(n) > lease.gen + 1) gen.get(n) else lease.gen + 1),
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' = lastFetchedKey.put(n, leaseKey(lease)),
    // merged_bug_180 ground truth, judged on CONTENT regardless of
    // what the evidence cursor keys on: consuming "evidence" while the
    // holder-authored content never moved past this node's last
    // completed fetch means the consumed signal was foreign rv churn.
    evidenceFabricated' = or {
      evidenceFabricated,
      MAX_FOREIGN_BUMPS > 0 and lease.content == lastFetchedContent.get(n),
    },
    // Live latch (quint-policy P4): the consumption event is THIS
    // action's firing; evidenceEnabled(n) is its own state-dependent
    // enabling condition (true at every firing), so the latch carries
    // a real state-reading writer instead of a const toggle.
    evidenceConsumed' = or {
      evidenceConsumed,
      MAX_FOREIGN_BUMPS > 0 and evidenceEnabled(n),
    },
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, lease.content),
    foreignBumps' = foreignBumps,
    // Own-commit evidence is THE resolving completed read: both
    // act-failed legs (evidence re-acquire and believing-observe)
    // route through the production funnel, whose on_observed_held
    // clears the episode latch (merged_bug_002) -- the deferred 409's
    // question is answered. Guard idiom keeps gate-off regimes
    // state-space-identical.
    conflictDeferredV' =
      if (not(HOLDER_EVIDENCE_LOSE)) conflictDeferredV
      else conflictDeferredV.put(n, false),
    deferralResolvedV' =
      if (not(HOLDER_EVIDENCE_LOSE)) deferralResolvedV
      else deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    snap' = snap.put(n, Snap(lease)),
    held' = trackHeld(held, n, true),
    blindHolderFor' = 0,  // the holder re-believes; its entry is consumed
    clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
    genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited, drops' = drops,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
  }

  /// The believing holder=self completed read with NOTHING to consume
  /// (bug_002, production route_act_failed_read's FunnelResolve
  /// cells): the act phase died after a completed GET that still
  /// names this node holder, and own-commit evidence is NOT
  /// consumable (frozen content past the last completed fetch, or an
  /// empty unconfirmed ledger). Routed through the observation
  /// funnel, whose on_observed_held clears the episode latch — the
  /// deferred 409's question is answered. NO fence stamp (frozen
  /// content stamps NOTHING — sched.lease.cancelled-write+2's law is
  /// untouched: fence' = fence) and NO ledger consume; the read
  /// re-baselines the evidence cursor exactly as production
  /// content_baseline := this read's content. Enabled only under
  /// HOLDER_EVIDENCE_LOSE, so every gate-off regime is state-space
  /// byte-identical by construction (the action never fires there).
  /// Pre-change, this read was NOT expressible as a deferral-
  /// resolving step — noStaleExhaustedLose was green-by-omission over
  /// exactly the production leak (the model shared the blind spot).
  // r[verify sched.lease.holder-evidenced-lose+4]
  action actFailedHeldReadObserved(n: Node): bool = all {
    HOLDER_EVIDENCE_LOSE,
    alive.get(n),
    pending.get(n) == NoPending,
    leading.get(n),
    lease.holder == Held(n),
    not(evidenceEnabled(n)),
    // Frozen content stamps NOTHING: no fence stamp, no belief
    // change, no generation movement, no ledger consume.
    fence' = fence,
    leading' = leading,
    gen' = gen,
    unconfirmed' = unconfirmed,
    lastFetchedKey' =
      if (MAX_DROPS == 0) lastFetchedKey
      else lastFetchedKey.put(n, leaseKey(lease)),
    lastFetchedContent' =
      if (MAX_FOREIGN_BUMPS == 0) lastFetchedContent
      else lastFetchedContent.put(n, lease.content),
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    foreignBumps' = foreignBumps,
    // The funnel resolution (bug_002): a completed read of a lease we
    // hold answers a pending 409's question; the resolved-since ghost
    // records it for the staleness latch.
    conflictDeferredV' = conflictDeferredV.put(n, false),
    deferralResolvedV' =
      deferralResolvedV.put(
        n, (deferralResolvedV.get(n) or conflictDeferredV.get(n))
      ),
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    snap' = snap.put(n, Snap(lease)),
    held' = trackHeld(held, n, true),
    blindHolderFor' = bhNext(
      lease, leading, alive, unconfirmed,
      (if (MAX_DROPS == 0) lastFetchedKey else lastFetchedKey.put(n, leaseKey(lease))),
      blindHolderFor, false
    ),
    clocks' = clocks, lease' = lease, alive' = alive, obs' = obs,
    genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, exited' = exited, drops' = drops,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
  }

  /// Foreign non-protocol Lease write (merged_bug_180's fault axis): a
  /// third-party annotation/label patch bumps metadata.resourceVersion
  /// while the holder-authored spec content is untouched. Invalidates
  /// in-flight protocol snapshots exactly like any other write (the
  /// CAS guard keys on rv), but MUST move neither the observed-record
  /// clock nor the evidence cursor — both key on content.
  action bumpRv = all {
    MAX_FOREIGN_BUMPS > 0,
    foreignBumps < MAX_FOREIGN_BUMPS,
    lease.rv < MAX_RV,
    lease' = { ...lease, rv: lease.rv + 1 },
    foreignBumps' = foreignBumps + 1,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    clocks' = clocks, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, fence' = fence, gen' = gen,
    genHW' = genHW, genHWHolder' = genHWHolder,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    restores' = restores, stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed,
    lastFetchedKey' = lastFetchedKey,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    drops' = drops,
    blindHolderFor' = bhNext({ ...lease, rv: lease.rv + 1 }, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
  }

  /// Shutdown fault: SIGTERM. The loop breaks and runs the release
  /// gate — THE FIX ENCODED: release iff the node HELD the lease
  /// unsuperseded (LeaseStanding.should_release_on_shutdown), NEVER
  /// the belief the self-fence cleared (the calibration override
  /// lease-387-belief-gate swaps the gate and must violate
  /// gracefulHandover). A release clears the holder and bumps the rv,
  /// so a standby's next GET sees an empty holder and steals on its
  /// poll tick instead of waiting out the staleness threshold. The
  /// fenceThenExit latch records the interesting shape: exiting
  /// not-believing while the apiserver still names us.
  action shutdown(n: Node): bool = {
    // The release predicate — THE FIX: hold-gated. The latch below
    // fires iff the node exits while the GATE'S OWN SPEC says release
    // (held ∧ holder == us) and the release did not run: identically
    // false here by construction, and exactly the fence-then-SIGTERM
    // shape under the belief-gated calibration override
    // (lease-387-belief-gate), whose shutdownBeliefGated releases on
    // `leading` instead. Deliberately NOT "holder == us and no
    // release": a crash-restarted process whose previous incarnation's
    // lease survives has held = false by the gate's spec (it never
    // acquired THIS incarnation; sched.lease.graceful-release+2 gates
    // on "ever acquired and not since observed another holder") — its
    // skipped release costs the steal threshold, never correctness,
    // matching production.
    val releases = held.get(n) and lease.holder == Held(n)
    all {
    shutdowns < MAX_SHUTDOWNS,
    alive.get(n),
    not(exited.get(n)),
    pending.get(n) == NoPending,
    // State-space bound, as a PRECONDITION (the steal convention): the
    // release PUT bumps the rv, so a shutdown at the rv ceiling is the
    // exploration boundary — disabling loses nothing.
    lease.rv < MAX_RV,
    // Parentheses load-bearing on both latches (see steal's precedent).
    releaseSkippedWhileHolder' = (releaseSkippedWhileHolder or
      (held.get(n) and lease.holder == Held(n) and not(releases))),
    fenceThenExit' = (fenceThenExit or
      (held.get(n) and not(leading.get(n)) and lease.holder == Held(n))),
    lease' = if (releases)
               { holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1, gen: lease.gen }
             else lease,
    alive' = alive.put(n, false),
    exited' = exited.put(n, true),
    leading' = leading.put(n, false),
    held' = trackHeld(held, n, false),
    shutdowns' = shutdowns + 1,
    snap' = snap.put(n, NoSnap),
    obs' = obs.put(n, NoObs),
    clocks' = clocks, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace, deletes' = deletes, delVictims' = delVictims,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, suspends' = suspends,
    suspendStraddled' = suspendStraddled,
    // The exiting process takes its in-memory ledger and cursor with
    // it (terminal: exited nodes never act again).
    unconfirmed' = unconfirmed.put(n, NoUnconf),
    lastFetchedKey' = lastFetchedKey.put(n, -1),
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(
      if (releases) { holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1, gen: lease.gen } else lease,
      leading.put(n, false), alive.put(n, false),
      unconfirmed.put(n, NoUnconf), lastFetchedKey,
      blindHolderFor, false),
    }
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
  /// it: the next steal of the recreated lease seeds past the floor
  /// (genHW+1) unless the ledger row at the floor is the stealer's own,
  /// so a foreign or fresh stealer exceeds every generation ever
  /// acquired, even one whose holder never dispatched anything, while
  /// the deposed holder itself re-acquires its own epoch (the retain arm
  /// the noRetainAfterDeletion witness pins).
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
    lease' = { holder: NoHolder, rv: lease.rv + 1, content: lease.content + 1, gen: 0 },
    deletes' = deletes + 1,
    delVictims' = match lease.holder {
      | NoHolder => delVictims
      | Held(h) => delVictims.union(Set(h))
    },
    clocks' = clocks, alive' = alive, leading' = leading, snap' = snap,
    obs' = obs, fence' = fence, gen' = gen, genHW' = genHW,
    genHWHolder' = genHWHolder, acquiredAt' = acquiredAt,
    casRace' = casRace,
    claimFailures' = claimFailures, restores' = restores,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = 0,  // recreated empty: nobody is named holder
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
    genHWHolder' = NoHolder,
    restores' = restores + 1,
    clocks' = clocks, lease' = lease, alive' = alive, leading' = leading,
    snap' = snap, obs' = obs, fence' = fence, gen' = gen,
    acquiredAt' = acquiredAt, casRace' = casRace, deletes' = deletes,
    delVictims' = delVictims, claimFailures' = claimFailures,
    stealRetained' = stealRetained,
    stealBumpedForeignTie' = stealBumpedForeignTie,
    renewBumpedAboveEntry' = renewBumpedAboveEntry,
    pending' = pending, held' = held, exited' = exited,
    suspends' = suspends, shutdowns' = shutdowns,
    releaseSkippedWhileHolder' = releaseSkippedWhileHolder,
    suspendStraddled' = suspendStraddled, fenceThenExit' = fenceThenExit,
    unconfirmed' = unconfirmed, lastFetchedKey' = lastFetchedKey,
    drops' = drops,
    foreignBumps' = foreignBumps,
    conflictDeferredV' = conflictDeferredV,
    deferralResolvedV' = deferralResolvedV,
    staleExhaustedLose' = staleExhaustedLose,
    blindConflictLose' = blindConflictLose,
    acquireInstV' = acquireInstV,
    stepDownReqV' = stepDownReqV,
    stepDownsV' = stepDownsV,
    staleStepDownServed' = staleStepDownServed,
    lastFetchedContent' = lastFetchedContent,
    evidenceFabricated' = evidenceFabricated,
    evidenceConsumed' = evidenceConsumed,
    blindHolderFor' = bhNext(lease, leading, alive, unconfirmed, lastFetchedKey, blindHolderFor, false),
  }

  // --- 7. init ----------------------------------------------------------
  /// gen[n] = 1 mirrors the production `AtomicU64::new(1)`
  /// (rio-scheduler/src/main.rs, rio-controller/src/main.rs). genHW = 0
  /// is an empty PG. lease.gen = 0 is `lease_transitions` before any
  /// holder.
  action init = all {
    clocks' = NODES.mapBy(n => 0),
    lease' = { holder: NoHolder, rv: 0, content: 0, gen: 0 },
    alive' = NODES.mapBy(n => true),
    leading' = NODES.mapBy(n => false),
    snap' = NODES.mapBy(n => NoSnap),
    obs' = NODES.mapBy(n => NoObs),
    fence' = NODES.mapBy(n => 0),
    gen' = NODES.mapBy(n => 1),
    genHW' = 0,
    genHWHolder' = NoHolder,
    acquiredAt' = NODES.mapBy(n => NO_ACQ),
    casRace' = false,
    deletes' = 0,
    delVictims' = NODES.filter(n => false),
    claimFailures' = 0,
    restores' = 0,
    stealRetained' = false,
    stealBumpedForeignTie' = false,
    renewBumpedAboveEntry' = false,
    pending' = NODES.mapBy(n => NoPending),
    held' = NODES.mapBy(n => false),
    exited' = NODES.mapBy(n => false),
    suspends' = 0,
    shutdowns' = 0,
    releaseSkippedWhileHolder' = false,
    suspendStraddled' = false,
    fenceThenExit' = false,
    unconfirmed' = NODES.mapBy(n => NoUnconf),
    lastFetchedKey' = NODES.mapBy(n => -1),
    blindHolderFor' = 0,
    drops' = 0,
    foreignBumps' = 0,
    lastFetchedContent' = NODES.mapBy(_ => -1),
    evidenceFabricated' = false,
    evidenceConsumed' = false,
    conflictDeferredV' = NODES.mapBy(_ => false),
    deferralResolvedV' = NODES.mapBy(_ => false),
    staleExhaustedLose' = false,
    blindConflictLose' = false,
    acquireInstV' = NODES.mapBy(_ => 0),
    stepDownReqV' = NODES.mapBy(_ => NoStepDown),
    stepDownsV' = 0,
    staleStepDownServed' = false,
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
  action loseObservedAny = {
    nondet n = NODES.oneOf()
    loseObserved(n)
  }
  action requestStepDownAny = {
    nondet n = NODES.oneOf()
    requestStepDown(n)
  }
  action serveStepDownAny = {
    nondet n = NODES.oneOf()
    serveStepDown(n)
  }
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
  /// One holder's split renew commits at send (suspend fault).
  action renewSendSteadyAny = {
    nondet n = NODES.oneOf()
    renewSendSteady(n)
  }
  /// One parked read completes (suspend fault).
  action renewReadOkAny = {
    nondet n = NODES.oneOf()
    renewReadOk(n)
  }
  /// One node receives SIGTERM (shutdown fault).
  action shutdownAny = {
    nondet n = NODES.oneOf()
    shutdown(n)
  }
  /// One holder-shaped PUT commits while its round fails after
  /// transmission (dropped-write fault).
  action renewSendDroppedAny = {
    nondet n = NODES.oneOf()
    renewSendDropped(n)
  }
  action renewSendLostAny = {
    nondet n = NODES.oneOf()
    renewSendLost(n)
  }
  /// One node's fetch observes its own committed-but-unconfirmed
  /// write (dropped-write fault's evidence rule).
  action fetchObservesOwnCommitAny = {
    nondet n = NODES.oneOf()
    fetchObservesOwnCommit(n)
  }

  /// One node's evidence-free believing holder=self completed read
  /// (bug_002 FunnelResolve cells).
  action actFailedHeldReadObservedAny = {
    nondet n = NODES.oneOf()
    actFailedHeldReadObserved(n)
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
    // Fault-gated (disabled at MAX_SUSPENDS = MAX_SHUTDOWNS = 0, so the
    // four pre-existing regimes' transition relations — and state
    // spaces — are unchanged; re-measured at the introducing commit).
    renewSendSteadyAny,
    renewReadOkAny,
    shutdownAny,
    // Fault-gated (disabled at MAX_DROPS = 0 -- same discipline as
    // the suspend/shutdown pair above; the six pre-existing regimes'
    // state spaces re-measured byte-identical at the introducing
    // commit).
    renewSendDroppedAny,
    fetchObservesOwnCommitAny,
    // Fault-gated (disabled at MAX_FOREIGN_BUMPS = 0 -- same
    // discipline; pre-existing regimes re-measured byte-identical at
    // the introducing commit).
    renewSendLostAny,
    bumpRv,
    // Fault/plane-gated (disabled at HOLDER_EVIDENCE_LOSE = false /
    // MAX_STEP_DOWNS = 0 -- same discipline as the entries above;
    // pre-existing regimes' state spaces re-measured byte-identical
    // at the introducing commit).
    loseObservedAny,
    requestStepDownAny,
    serveStepDownAny,
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
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
//   - renewLease's claim-path arm (seedFor bumping to genHW + 1 when the
//     floor exceeds the entry) is LOAD-BEARING here
//     and only here: after a deletion the recreated lease's transition
//     count no longer encodes a recovered holder's generation, so the
//     entry half alone under-restores it to the deletion victim's
//     generation. The noFloorBumpAfterDeletion witness check is the
//     standing evidence. See renewLease's doc comment, case 3.
//   - steal's own-row-tie retain arm (seedFor returning the entry when
//     the ledger row at the floor is the stealer's own) fires in this
//     regime and only this regime -- the noRetainAfterDeletion witness
//     check is the standing evidence -- so the fetch-max-seed /
//     generation-claim retain clause is verified against an explored
//     path, not assumed.
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
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
// steal's claim target is seedFor over the entry max(gen[n],
// lease.gen + 2): it exceeds the PG floor unless the ledger row at the
// floor is the stealer's own; when the floor term lies (a failed claim
// left it behind, a restore zeroed it), the lease-derived entry still
// increases strictly across successive stealers, because only a steal
// bumps the transition count and nothing in the PG fault set resets it. Each PG fault alone is survivable. A
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
//   the collision and must be revisited -- that is the conjunction half
//   of this regime's non-vacuity evidence (the faults are load-bearing
//   in conjunction with the deletion). If the MAX_DELETES = 0 run ever
//   FAILS, a PG fault alone breaks the fence with no operator action: a
//   new protocol finding, not a tuning problem.
//
// SINGLE-FAULT REACHABILITY at these constants is machine-checked: the
// expect-violation checks quint-leader-election-witness-claim-failure
// (noClaimFailure) and quint-leader-election-witness-restore-reclaim
// (noReclaimAfterRestore) in nix/quint.nix fail if a failed claim
// INSERT, or a restore followed by a successful re-claim, ever stops
// being reachable in this regime's explored space. The conjunction
// runs above remain a manual procedure because they require
// re-enabling MAX_DELETES -- different constants than this shipped
// module.
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
    MAX_RESTORES = 1,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*
}

// =========================================================================
// The suspend regime: the split-renew fault enabled (MAX_SUSPENDS = 1)
// over the asymmetric-TTL constants. A steady-state renew's PUT commits
// at send while the loop parks on the response read — a host suspend
// straddling an in-flight round-trip — and the parked node is exempt
// from the loop-discipline tick cap for exactly that interval (the
// honest statement of bug_096: the loop-interval assumption never held
// across a parked await).
//
// The operative property here is boundedDualLeadership, NOT neverDual:
// a parked believer with a thief past its steal threshold is a genuine
// dual-belief state — reachable under EITHER anchoring — because the
// believer takes no actions (and so discovers nothing) while parked.
// What the anchoring decides is whether the dual is *bounded*: with the
// fence restarting at the attempt-start ANCHOR (renewReadOk — the
// production RenewAnchor encoding), the post-read believer is at or
// past its own fence deadline, so discovery disjunct (1) is armed and
// boundedDualLeadership HOLDS; with response anchoring (the
// lease-096-response-anchor calibration override), the post-read
// believer's fence is fresh, its snapshot is spent, and NO discovery
// path is armed — boundedDualLeadership is VIOLATED. The neverDual
// exclusion rationale (deviation (a), relocated from the retired lease
// invariant map): a parked believer (a host suspended mid-await holds
// is_leader frozen — it takes no actions, discovers nothing) plus a
// thief past its steal threshold is a GENUINE dual-belief state,
// reachable under EITHER anchoring; what the anchoring decides is
// BOUNDEDNESS — with attempt-start anchoring the post-read believer is
// at/past its own fence deadline, discovery disjunct (1) is armed and
// boundedDualLeadership HOLDS; this calibration shows response
// anchoring breaks exactly that. The generation fence
// (sched.lease.generation-fence+3) remains the correctness backstop
// throughout — the blast radius of every lease-side property here is
// ops/availability, not execution correctness.
//
// Non-vacuity: the noSuspendStraddle witness (expect-violation,
// quint-leader-election-witness-suspend-straddle) proves the straddle
// scenario is actually explored.
module leaderElectionSuspend {
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 1,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// The suspend-straddle scenario, end to end: n1 acquires, its next
  /// steady renew commits at send (anchor = clock 0), the host
  /// suspends — three ticks pass while the read is parked and the tick
  /// cap is exempt — the read completes (fence := anchor, straddle
  /// latched), and the very next fence check fires: the tick cap now
  /// blocks n1's clock, selfFence is the enabled action, and the
  /// zombie leader is gone at the first post-resume check. Under
  /// response anchoring the same schedule leaves n1 leading with a
  /// fresh fence — the calibration override's falsification.
  run suspendStraddleFencesRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(apiGet("n1"))
      .then(renewSendSteady("n1"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(renewReadOk("n1"))
      .then(selfFence("n1"))
      .expect(all {
        not(leading.get("n1")),
        suspendStraddled,
        lease.holder == Held("n1"),  // the fence is local; the lease still names n1
      })
}

// =========================================================================
// The shutdown regime: the graceful-exit fault enabled
// (MAX_SHUTDOWNS = 1) over the asymmetric-TTL constants, verifying the
// belief/hold split (bug_387). The release gate is HOLD-gated
// (LeaseStanding.should_release_on_shutdown): a self-fence clears only
// belief, so fence-then-SIGTERM still releases the lease the apiserver
// still attributes to us — gracefulHandover is the headline invariant,
// and neverDual carries over from the asymmetric regime (an exited
// node is not a believer; the early release only ACCELERATES the
// successor, never overlaps it).
//
// Non-vacuity: the noFenceThenExit witness (expect-violation,
// quint-leader-election-witness-fence-then-exit) proves the
// fence-then-SIGTERM shape is actually explored; the
// lease-387-belief-gate calibration override swaps the gate back to
// belief and must violate gracefulHandover.
module leaderElectionShutdown {
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 1,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// Fence-then-SIGTERM releases: n1 acquires, goes blind past its
  /// fence deadline, self-fences (belief gone, HOLD intact), and the
  /// SIGTERM release gate still clears the holder — the successor
  /// steals on its next poll instead of waiting out the staleness
  /// threshold. Under the belief-gated release (the calibration
  /// override) the same schedule skips the release and latches
  /// releaseSkippedWhileHolder.
  run fenceThenSigtermReleasesRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(selfFence("n1"))
      .then(shutdown("n1"))
      .expect(all {
        lease.holder == NoHolder,
        exited.get("n1"),
        not(leading.get("n1")),
        fenceThenExit,
        gracefulHandover,
      })

  /// The legitimate skip: n1 is deposed by a steal it OBSERVES (a
  /// completed round resolving not-leading clears the hold), so its
  /// SIGTERM touches nothing — the thief's lease survives.
  run conflictThenSigtermSkipsRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(apiGet("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(selfFence("n1"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(tick("n1")).then(tick("n2"))
      .then(steal("n2"))
      .then(apiGet("n1"))
      .then(shutdown("n1"))
      .expect(all {
        lease.holder == Held("n2"),
        exited.get("n1"),
        gracefulHandover,
      })
}

// =========================================================================
// The dropped-write (mid-band) fault regime: merged_bug_303. The
// environment is the mid-band latency band -- the fetch phase of every
// round fits its budget, the act's response never arrives, so COMPLETE
// renew rounds do not exist (renewLease is absent from the step) while
// transmitted PUTs still commit (renewSendDropped). Steals stay in: the
// thief is a different pod on a different network path, and a healthy
// thief against a blind victim is exactly the adversarial shape
// neverDual must survive. Asymmetric-TTL constants (the healthy
// separation: STEAL_AFTER - FENCE_AFTER = 3 >= RENEW + 2*MAX_SKEW), so
// neverDual is checked here IN ADDITION to leaderlessBounded --
// neverDual is the SOUNDNESS ARBITER for the evidence rule: stamping
// the fence at the ledger anchor must never let a blind victim's
// belief outlive the thief's steal deadline. If neverDual falls here
// the fix design is wrong -- stop and re-derive the anchor rule, never
// widen constants.
//
// BLIND_HOLDER_BOUND = 8 is a calibrated, boundary-measured value: the
// forcing cap parks the blind node's clock at anchor + FENCE_AFTER +
// RENEW (3 ticks of window), and the peer may start MAX_SKEW behind
// the anchor and drag to blind + MAX_SKEW (5 ticks), so the maximal
// open stretch is 8 global ticks. Measured from both sides at the
// introducing commit: 7 violates (the 3+5 schedule), 8 holds. The
// lease-303-blind-timeout calibration twin (the evidence fetch
// removed, the pre-fix GET restored, the forcing cap dropped)
// stretches the window to the trace horizon and violates the bound --
// the per-cycle livelock evidence.
// MAX_TIME = 10 covers a drop, a fence, the forced evidence poll, and
// a second drop cycle within the skew choreography.
// =========================================================================
module leaderElectionDroppedWrite {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 10,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 2,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// The mid-band step: no complete renew rounds (the regime's
  /// premise), no crash/recover (fault economy -- the pre-existing
  /// regimes own those conjunctions), everything else the live
  /// model's.
  action midBandStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    conflictAny,
    selfFenceAny,
    renewSendDroppedAny,
    fetchObservesOwnCommitAny,
    // Fault-gated (disabled at MAX_FOREIGN_BUMPS = 0 -- same
    // discipline; pre-existing regimes re-measured byte-identical at
    // the introducing commit).
    renewSendLostAny,
    bumpRv,
  }

  // --- deterministic protocol runs ---------------------------------------
  /// The livelock broken, end to end: the holder's writes drop TWICE
  /// (committing both times, bumping the rv both times) and the
  /// belief survives BOTH on own-commit evidence alone -- the fence
  /// never fires, no leaderless window ever opens, and each stamp
  /// lands at the LEDGER's anchor (not the read instant). Pre-fix
  /// (the lease-303-blind-timeout twin) the same schedule fences the
  /// holder and the committed garbage writes hold every standby's
  /// staleness clock hostage.
  run livelockBrokenRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(apiGet("n1"))
      .then(renewSendDropped("n1"))
      .then(tick("n1"))
      .then(fetchObservesOwnCommit("n1"))
      .then(renewSendDropped("n1"))
      .then(tick("n2"))
      .then(tick("n1"))
      .then(fetchObservesOwnCommit("n1"))
      .expect(all {
        leading.get("n1"),
        unconfirmed.get("n1") == NoUnconf,
        fence.get("n1") == 1,
        lease.rv == 3,
        lease.holder == Held("n1"),
        blindHolderFor == 0,
        drops == 2,
        not(dual),
      })
}

// =========================================================================
// The foreign-rv-writer fault regime (bughunt3-wave S6b
// merged_bug_180): the mid-band world of droppedWrite PLUS a foreign
// non-protocol Lease mutator (bumpRv: annotation/label patches that
// bump metadata.resourceVersion without touching the holder-authored
// spec content). The observed record and the evidence cursor key on
// CONTENT (the fix), so:
//   - the staleness clock keeps aging under foreign churn (a dead
//     leader is stolen on schedule — the steal guard's content
//     identity);
//   - own-commit evidence requires a CONTENT move (no fabrication
//     from foreign churn — noFabricatedEvidence, this regime's
//     headline);
// while the CAS guard still keys on rv, so foreign writes ALSO
// adversarially 409 in-flight protocol PUTs.
// The paired pin is quint-lease-calib-180-rv-keyed (OBS_KEY_RV
// restores the pre-fix keying; noFabricatedEvidence must fall).
module leaderElectionForeignRv {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 10,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 2,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// The mid-band step plus the foreign mutator (same fault economy
  /// as midBandStep — see leaderElectionDroppedWrite).
  action foreignRvStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    conflictAny,
    selfFenceAny,
    renewSendDroppedAny,
    renewSendLostAny,
    fetchObservesOwnCommitAny,
    bumpRv,
  }
}

// =========================================================================
// The holder-evidenced-lose regime (bughunt-4 S2, signed Q3): the
// foreign-rv fault economy with BOTH non-lose rv-movers live -- own
// dropped-then-committed writes (MAX_DROPS) and foreign metadata
// patches (MAX_FOREIGN_BUMPS) -- and the deferral law enabled. The
// operative properties: loseRequiresHolderEvidence (the blind-lose
// latch never fires in this world -- only the twin's toggle can write
// it), plus the inherited safety set (the deferral must not erode
// neverDual's separation argument: a deferred believer keeps belief at
// most one extra completed round, and FENCE/STEAL = 2/5 leaves the
// margin the production constants' derivation requires).
// Non-vacuity: noDeferredConflict is the reachability witness (expect
// [violation] -- a believing 409 actually defers here); the
// renewLease/loseObserved resolutions are exercised by the same
// schedules that reach the deferral.
// STATE-SPACE BASELINE: measurements live in the check transcript and
// the introducing commit message (this module's first wiring), per the
// house convention above.
module leaderElectionHolderEvidence {
  import leaderElection(
    NODES = Set("n1", "n2"),
    FENCE_AFTER = 2,
    STEAL_AFTER = 5,
    RENEW = 1,
    MAX_SKEW = 1,
    MAX_TIME = 10,
    MAX_GEN = 4,
    MAX_RV = 8,
    MAX_DELETES = 0,
    MAX_CLAIM_FAILURES = 0,
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    // ONE dropped-commit (vs the foreign-rv sibling's 2): the
    // deferral law's fault menu needs one zombie (the own-commit
    // rv-mover and its evidence path) plus two foreign patches (a
    // deferral and an exhaustion without any holder change). The
    // double-drop interplay -- two unconfirmed writes, the
    // oldest-anchor ledger law -- is the droppedWrite regime's
    // domain and stays at MAX_DROPS = 2 there. At 2 drops this
    // regime exceeded 720M generated states without exhausting
    // (measured before the trim); at 1 drop it is exhaustively
    // checkable inside a wired gate budget.
    MAX_DROPS = 1,
    MAX_FOREIGN_BUMPS = 2,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 8,
    HOLDER_EVIDENCE_LOSE = true,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 0,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// foreignRvStep plus the holder-evidenced resolution path.
  action holderEvidenceStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    loseObservedAny,
    selfFenceAny,
    renewSendDroppedAny,
    renewSendLostAny,
    fetchObservesOwnCommitAny,
    actFailedHeldReadObservedAny,
    bumpRv,
  }

  /// Constructor-reachability probe (bug_002): the evidence-free
  /// holder=self act-failed read FIRES with a pending deferral and
  /// resolves it — n1 acquires, a foreign rv bump 409s its renew (the
  /// one-round deferral), the act-failed frozen-content read clears
  /// the latch and records the resolution, and the NEXT believing 409
  /// defers fresh instead of exhausting (the loop red
  /// act_failed_frozen_content_read_resolves_the_deferral, at model
  /// grain). staleExhaustedLose stays down throughout: the 409s were
  /// not consecutive.
  run actFailedFrozenReadResolvesRun =
    init
      .then(apiGet("n1"))
      .then(steal("n1"))
      .then(apiGet("n1"))
      .then(bumpRv)
      .then(conflict("n1"))
      .then(actFailedHeldReadObserved("n1"))
      .expect(all {
        leading.get("n1"),
        not(conflictDeferredV.get("n1")),
        deferralResolvedV.get("n1"),
        not(staleExhaustedLose),
      })
      .then(apiGet("n1"))
      .then(bumpRv)
      .then(conflict("n1"))
      .expect(all {
        leading.get("n1"),
        conflictDeferredV.get("n1"),
        not(staleExhaustedLose),
      })
}

// =========================================================================
// The cooperative step-down regime (bughunt-4 S2, merged_bug_128): the
// base fault economy plus the request/serve pair, instance-keyed (the
// production law). The operative property: noStaleStepDownServed -- a
// step-down is served only by the tenure instance that filed it; the
// false-alarm fence -> same-epoch re-acquire pair clears a stale
// request instead of inheriting it. The COUNT_KEYED twin restores the
// transition-count stamp + no-clear world and falsifies it.
// Non-vacuity: noStepDownRequested is the reachability witness (expect
// [violation] -- requests are actually filed); legitimate same-
// instance serves are reachable (the request->serve pair with no
// intervening edge), so the law is exercised, not vacuously held.
module leaderElectionStepDown {
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
    MAX_RESTORES = 0,
    MAX_SUSPENDS = 0,
    MAX_SHUTDOWNS = 0,
    MAX_DROPS = 0,
    MAX_FOREIGN_BUMPS = 0,
    OBS_KEY_RV = false,
    BLIND_HOLDER_BOUND = 0,
    HOLDER_EVIDENCE_LOSE = false,
    CONFLICT_IMMEDIATE_LOSE = false,
    MAX_STEP_DOWNS = 2,
    STEP_DOWN_COUNT_KEYED = false
  ).*

  /// The base step plus the step-down pair.
  action stepDownStep = any {
    tickAny,
    apiGetAny,
    stealAny,
    renewLeaseAny,
    conflictAny,
    selfFenceAny,
    crashAny,
    recoverAny,
    requestStepDownAny,
    serveStepDownAny,
  }
}
