---------------------------- MODULE LeaderElection ----------------------------
(***************************************************************************)
(* Formal model of rio-lease's apiserver-CAS leader election protocol.     *)
(*                                                                         *)
(* Formalizes:                                                             *)
(*   - sched.lease.at-most-one-leader  (the safety invariant)              *)
(*   - sched.lease.k8s-lease           (the protocol mechanism)            *)
(*                                                                         *)
(* From docs/spec/components/scheduler.typ. Update both when the protocol  *)
(* changes -- `tracey bump` on the rule will flag the r[verify] marker in  *)
(* nix/tla.nix as stale.                                                   *)
(*                                                                         *)
(* SCOPE NOTE -- this is the Phase-0 spike model. It does NOT model:       *)
(*   - the observed-record clock (rv-change staleness detection)           *)
(*   - self-fencing on missed renew                                        *)
(*   - leader crash / restart / network partition                          *)
(* Nodes can only Steal when they observe an empty lease (holder = NULL).  *)
(* This is enough to capture the kube-leader-election 0.43 bug class       *)
(* (no CAS precondition on replace() -> all racers believe they won). The  *)
(* full model with explicit time and self-fence is Phase 1.                *)
(*                                                                         *)
(* The model includes the CAS precondition (Replace(n) checks lease.rv =   *)
(* snap[n].rv). Removing it reproduces the kube-leader-election 0.43 bug:  *)
(* TLC finds a dual-leadership counterexample at depth 5 -- both nodes GET *)
(* an empty lease, both Steal on stale snapshots, both succeed, both       *)
(* Leading.                                                                *)
(*                                                                         *)
(* SPIKE LIMITATION: with steal-only-when-empty and no time model, once    *)
(* any node wins the first race the lease holder never returns to NULL     *)
(* and AtMostOneLeader holds partly vacuously (there's only one contention *)
(* window -- the race for the initial empty lease). After that, the only   *)
(* writer that can pass the CAS is the leader itself, so Observe(n) and    *)
(* the 409 branch of Renew(n) are unreachable. Real contention coverage    *)
(* (deposed leader, repeated steals) requires the Phase-1 model with the   *)
(* observed-record clock and self-fence TTL.                               *)
(*                                                                         *)
(* INVARIANT NAMING -- sched.lease.at-most-one-leader+2 names the model's  *)
(* invariants AtMostOneCASWinner (the hard half: two Replace actions       *)
(* cannot both succeed at the same rv) and BoundedDualLeadership (the soft *)
(* half: if two replicas concurrently believe they lead, the older one is  *)
(* past its self-fence deadline). The spike's AtMostOneLeader is a         *)
(* degenerate combination of both -- the CAS half holds by construction    *)
(* of Replace(n), and the dual-belief window is empty because the spike    *)
(* has no time model so the self-fence and observed-clock paths that open  *)
(* it are unreachable. Phase-1 splits AtMostOneLeader into the two named   *)
(* invariants so the soft half is checked non-vacuously over the           *)
(* deposed-leader and crash/recovery interleavings.                        *)
(*                                                                         *)
(* The {Get, Steal, Renew, Observe, Discard} disjunction parallels the     *)
(* case structure of decide() in rio-lease/src/election.rs:                *)
(*   - Steal   <-> Decision::Steal   (snap.holder = NULL)                  *)
(*   - Renew   <-> Decision::Renew   (snap.holder = us)                    *)
(*   - Observe/Discard <-> Decision::Standby (snap.holder = other)         *)
(* Observe is the `was_leading && result != Leading` step-down edge in the *)
(* lease loop; Discard is the standby's no-op tick. A Kani harness on      *)
(* decide() (Phase-1, not yet landed) will verify the per-decision         *)
(* logic; this model verifies the protocol that calls it.                  *)
(*                                                                         *)
(* CORRESPONDENCE CAVEAT: the table above is exact only over reachable     *)
(* spike states. The TLA preconditions are coarser than decide()'s         *)
(* case split: Discard's `snap.holder /= NULL` admits `holder = n`         *)
(* (which decide() maps to Renew, not Standby), and Observe's              *)
(* `snap.holder /= n` admits `holder = NULL` (which decide() maps to       *)
(* Steal regardless of was_leading). Both edge cases are unreachable       *)
(* in the spike (a Standby never snapshots itself as holder; a             *)
(* Leading node never snapshots an empty lease), so the partition is       *)
(* sound here. A Phase-1 model with reachable deposed-leader states        *)
(* MUST refine Observe/Discard to match decide()'s exact three-way         *)
(* split: holder.is_empty() / holder == us / holder == other.              *)
(***************************************************************************)

EXTENDS Integers, FiniteSets, TLC

CONSTANTS Nodes,    \* set of node identities, e.g. {n1, n2, n3}
          NULL,     \* model value for "no holder" / "no snapshot"
          Ttl,      \* LEASE_TTL in ticks
          Renew,    \* RENEW_INTERVAL in ticks (Ttl/Renew ~= 3)
          MaxSkew,  \* clock skew bound
          MaxTime,  \* clock ceiling (state-space bound)
          MaxGen,   \* generation ceiling (state-space bound)
          MaxRv     \* rv ceiling (state-space bound)

ASSUME Ttl \in Nat /\ Renew \in Nat /\ Renew >= 1 /\ Ttl >= Renew
ASSUME MaxSkew \in Nat /\ MaxTime \in Nat /\ MaxGen \in Nat /\ MaxRv \in Nat

VARIABLES
  clocks,      \* [Nodes -> 0..MaxTime] -- per-node monotonic clock
  lease,       \* [holder: Nodes \cup {NULL}, rv: 0..MaxRv, gen: 0..MaxGen]
  alive,       \* [Nodes -> BOOLEAN] -- process running?
  state,       \* [Nodes -> {"Following", "Leading"}] -- belief
  snap,        \* [Nodes -> (LeaseRecord \cup {NULL})] -- last GET'd lease
  obs,         \* [Nodes -> (ObsRecord \cup {NULL})] -- observed-record clock
  fence,       \* [Nodes -> 0..MaxTime] -- last successful round-trip
  gen,         \* [Nodes -> 0..MaxGen] -- in-memory generation (Arc<AtomicU64>)
  genHW,       \* 0..MaxGen -- PG generation high-water mark (persistent)
  acquiredAt,  \* [Nodes -> (0..MaxRv \cup {NULL})] -- rv at last acquire
  casRace      \* BOOLEAN -- has any CAS race been observed?

vars == <<clocks, lease, alive, state, snap, obs, fence, gen, genHW, acquiredAt, casRace>>

LeaseRecord == [holder : Nodes \cup {NULL}, rv : 0..MaxRv, gen : 0..MaxGen]
ObsRecord   == [rv : 0..MaxRv, since : 0..MaxTime]

TypeOK ==
  /\ clocks \in [Nodes -> 0..MaxTime]
  /\ lease  \in LeaseRecord
  /\ alive  \in [Nodes -> BOOLEAN]
  /\ state  \in [Nodes -> {"Following", "Leading"}]
  /\ snap   \in [Nodes -> (LeaseRecord \cup {NULL})]
  /\ obs    \in [Nodes -> (ObsRecord \cup {NULL})]
  /\ fence  \in [Nodes -> 0..MaxTime]
  /\ gen    \in [Nodes -> 0..MaxGen]
  /\ genHW  \in 0..MaxGen
  /\ acquiredAt \in [Nodes -> (0..MaxRv \cup {NULL})]
  /\ casRace \in BOOLEAN

\* SYMMETRY: nodes are interchangeable. Cuts the state space by |Nodes|!.
perm == Permutations(Nodes)

\* The proof assumption: clocks are bounded-skew. The model does not explore
\* states that violate this; the proof claim is "safe under skew <= MaxSkew."
ClockSkewBound ==
  \A m, n \in Nodes : clocks[m] - clocks[n] <= MaxSkew

\* -----------------------------------------------------------------------
\* THE hard half of sched.lease.at-most-one-leader+2: the apiserver's
\* optimistic concurrency admits at most one writer per resourceVersion.
\* casRace is flipped by ReplaceGuard(n) when a PUT succeeds at an rv that
\* another currently-Leading node also acquired at -- i.e. the CAS let two
\* writers through. With the precondition (lease.rv = snap[n].rv) this is
\* unreachable: the second writer's snapshot is stale, its PUT 409s.
\* Without it (delete the precondition), both racers GET rv=R, both PUT,
\* both believe they won -- the kube-leader-election 0.43 bug.
\*
\* Deliberately-weakened test (run once during development, NOT in CI):
\* replacing `lease.rv = snap[n].rv` with TRUE in ReplaceGuard produces a
\* counterexample at depth 6: both nodes GET the empty lease at rv=0, n1
\* Steals (Leading, acquiredAt=0), n2 Steals at its stale rv=0 snapshot --
\* the PUT succeeds without the precondition, both Leading, casRace flips.
\* Both also reach gen=2: the generation fence cannot tell them apart
\* either. Recorded in the header (Task 9).
\* -----------------------------------------------------------------------
AtMostOneCASWinner == ~casRace

\* -----------------------------------------------------------------------
\* Initial state. gen[n] = 1 mirrors the production `AtomicU64::new(1)`
\* (rio-scheduler/src/main.rs:142, rio-controller/src/main.rs:313). genHW = 0
\* is an empty PG. lease.gen = 0 is `lease_transitions` before any holder.
\* -----------------------------------------------------------------------
Init ==
  /\ clocks = [n \in Nodes |-> 0]
  /\ lease  = [holder |-> NULL, rv |-> 0, gen |-> 0]
  /\ alive  = [n \in Nodes |-> TRUE]
  /\ state  = [n \in Nodes |-> "Following"]
  /\ snap   = [n \in Nodes |-> NULL]
  /\ obs    = [n \in Nodes |-> NULL]
  /\ fence  = [n \in Nodes |-> 0]
  /\ gen    = [n \in Nodes |-> 1]
  /\ genHW  = 0
  /\ acquiredAt = [n \in Nodes |-> NULL]
  /\ casRace = FALSE

\* -----------------------------------------------------------------------
\* Time. clocks[n] advances independently. Always enabled -- CLOCK_MONOTONIC
\* ticks regardless of process state (alive or not). Bounded skew is a state
\* CONSTRAINT, not a Tick precondition: it bounds how far one node's clock
\* can lag another's, not when Tick fires.
\* -----------------------------------------------------------------------
Tick(n) ==
  /\ clocks[n] < MaxTime
  /\ clocks' = [clocks EXCEPT ![n] = @ + 1]
  /\ UNCHANGED <<lease, alive, state, snap, obs, fence, gen, genHW, acquiredAt, casRace>>

\* -----------------------------------------------------------------------
\* Apiserver round-trip. Get(n) is the GET; Steal/RenewLease are the PUT
\* (each through ReplaceGuard); Conflict is the 409. All four reset fence[n]
\* -- any successful round-trip resets the self-fence clock (run_lease_loop()'s
\* renew arm: "the clock tracks 'am I blind', not 'am I leader'").
\* -----------------------------------------------------------------------

\* GET the lease. obs[n] update follows decide_pure()'s ObservedUpdate:
\*   - holder = NULL    -> Clear (steal now, observed-record meaningless)
\*   - holder = n       -> Keep (we hold it; observed tracks OTHER holders)
\*   - holder = other, same rv we observed -> Keep (clock still running)
\*   - holder = other, new rv (or first observation) -> StartObserving
Get(n) ==
  /\ alive[n]
  /\ snap'  = [snap EXCEPT ![n] = lease]
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ obs'   = [obs EXCEPT ![n] =
       IF lease.holder = NULL THEN NULL
       ELSE IF lease.holder = n THEN obs[n]
       ELSE IF obs[n] /= NULL /\ obs[n].rv = lease.rv THEN obs[n]
       ELSE [rv |-> lease.rv, since |-> clocks[n]]]
  /\ UNCHANGED <<clocks, lease, alive, state, gen, genHW, acquiredAt, casRace>>

\* The shared CAS guard for Steal/Renew. NOT a standalone action -- a helper
\* that Steal and Renew conjoin. The casRace history is recorded HERE: it's
\* the only place a CAS conflict can be observed. casRace flips when this
\* node's PUT succeeds at an rv that another currently-Leading node also
\* acquired at -- the apiserver let two writers through.
ReplaceGuard(n) ==
  /\ snap[n] /= NULL
  /\ snap[n].rv < MaxRv
  /\ lease.rv = snap[n].rv      \* THE CAS PRECONDITION (deliberately-weakened test 1, Task 4)
  /\ casRace' = (casRace \/ \E m \in Nodes \ {n} :
       alive[m] /\ state[m] = "Leading" /\ acquiredAt[m] = snap[n].rv)

\* The decide()->Steal path. Phase-1 expansion: empty-holder OR stale-observed
\* (the deposed-leader path the spike could not reach). gen[n] is set per
\* on_acquire() + seed_generation_from() collapsed: the in-memory fetch_add(1)
\* AND the PG fetch_max happen before recovery_complete=true, so the model
\* treats them as one atomic step. r[sched.recovery.fetch-max-seed].
Steal(n) ==
  LET seeded == IF gen[n] + 1 > genHW + 1 THEN gen[n] + 1 ELSE genHW + 1 IN
  /\ alive[n]
  /\ snap[n] /= NULL
  /\ \/ snap[n].holder = NULL
     \/ /\ snap[n].holder /= NULL /\ snap[n].holder /= n
        /\ obs[n] /= NULL /\ obs[n].rv = snap[n].rv
        /\ clocks[n] - obs[n].since > Ttl
  /\ ReplaceGuard(n)
  /\ lease.gen < MaxGen   \* state-space bound; rv bound is in ReplaceGuard
  \* gen[n] bound is a PRECONDITION (action disabled at the ceiling), not a
  \* saturating clamp -- saturation would let two nodes reach equal
  \* generations at MaxGen and falsify StaleLeaderHasStaleGeneration as a
  \* state-space artifact. Disabling loses nothing: a state where the next
  \* generation would exceed MaxGen is the exploration boundary, same as
  \* MaxTime for clocks.
  /\ seeded <= MaxGen
  /\ lease' = [holder |-> n, rv |-> snap[n].rv + 1, gen |-> lease.gen + 1]
  /\ state' = [state EXCEPT ![n] = "Leading"]
  /\ gen'   = [gen EXCEPT ![n] = seeded]
  /\ acquiredAt' = [acquiredAt EXCEPT ![n] = snap[n].rv]
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ obs'   = [obs   EXCEPT ![n] = NULL]      \* on_acquire clears observed
  /\ snap'  = [snap  EXCEPT ![n] = NULL]      \* spent the snapshot
  /\ UNCHANGED <<clocks, alive, genHW>>

\* The decide()->Renew path: we hold the lease, refresh renew_time. Bumps rv
\* (the apiserver bumps on every write); does NOT touch holder/gen.
\* Named RenewLease (not Renew) -- the constant Renew is RENEW_INTERVAL.
RenewLease(n) ==
  /\ alive[n]
  /\ snap[n] /= NULL /\ snap[n].holder = n
  /\ ReplaceGuard(n)
  /\ lease' = [lease EXCEPT !.rv = snap[n].rv + 1]
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ snap'  = [snap  EXCEPT ![n] = NULL]
  /\ UNCHANGED <<clocks, alive, state, obs, gen, genHW, acquiredAt>>

\* The 409 path: snap stale. Stamps fence (apiserver answered). If we were
\* Leading this is an explicit lose -- someone stole between our GET and PUT.
\* Production distinguishes 409-on-renew (lose) from 409-on-steal (never
\* led); both end up Following, both reset fence -- the model collapses them.
Conflict(n) ==
  /\ alive[n]
  /\ snap[n] /= NULL /\ lease.rv /= snap[n].rv
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ state' = [state EXCEPT ![n] = "Following"]
  /\ snap'  = [snap  EXCEPT ![n] = NULL]
  /\ UNCHANGED <<clocks, lease, alive, obs, gen, genHW, acquiredAt, casRace>>

\* Persist the in-memory generation to PG. The production leader writes its
\* generation during dispatch (after recovery_complete=true). The window
\* between Steal(n) and Persist(n) is real -- a leader that crashes in it
\* leaves genHW stale, and the next acquirer seeds from the old high-water.
\* That's the genuinely-untested interleaving Phase-1 reaches.
Persist(n) ==
  /\ alive[n]
  /\ state[n] = "Leading"
  /\ gen[n] > genHW
  /\ genHW' = gen[n]
  /\ UNCHANGED <<clocks, lease, alive, state, snap, obs, fence, gen, acquiredAt, casRace>>

\* -----------------------------------------------------------------------
\* Fault model. SelfFence is the production maybe_self_fence(); Crash and
\* Recover model a pod restart (k8s crash-recovery: in-memory state is
\* lost, the OS clock and the apiserver and PG persist, the identity --
\* the pod name -- survives).
\* -----------------------------------------------------------------------

\* maybe_self_fence(): a Leading node that has not had a successful
\* apiserver round-trip in > Ttl per its OWN clock flips is_leader=false
\* locally, without an apiserver write. The production loop checks this
\* every RENEW_INTERVAL; the model lets it fire any time the deadline has
\* passed (a superset of the production schedule -- sound for safety).
SelfFence(n) ==
  /\ alive[n]
  /\ state[n] = "Leading"
  /\ clocks[n] - fence[n] > Ttl
  /\ state' = [state EXCEPT ![n] = "Following"]
  /\ UNCHANGED <<clocks, lease, alive, snap, obs, fence, gen, genHW, acquiredAt, casRace>>

\* Pod crash. Loses ALL in-memory state: the belief (state), the snapshot,
\* the observed-record clock, the self-fence clock, the in-memory
\* generation (the Arc<AtomicU64> -- reset to the production
\* AtomicU64::new(1) init value, not 0), and the acquiredAt history. The
\* OS clock (clocks[n]), the apiserver (lease), and PG (genHW) persist.
Crash(n) ==
  /\ alive[n]
  /\ alive'      = [alive      EXCEPT ![n] = FALSE]
  /\ state'      = [state      EXCEPT ![n] = "Following"]
  /\ snap'       = [snap       EXCEPT ![n] = NULL]
  /\ obs'        = [obs        EXCEPT ![n] = NULL]
  /\ fence'      = [fence      EXCEPT ![n] = 0]
  /\ gen'        = [gen        EXCEPT ![n] = 1]
  /\ acquiredAt' = [acquiredAt EXCEPT ![n] = NULL]
  /\ UNCHANGED <<clocks, lease, genHW, casRace>>

\* Pod restart. The recovered process has no observation (its first
\* decide() returns StartObserving and waits a full Ttl before stealing)
\* unless the lease still carries its identity (decide() returns Renew --
\* the recovered leader re-acquires its own lease without contention).
Recover(n) ==
  /\ ~alive[n]
  /\ alive' = [alive EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<clocks, lease, state, snap, obs, fence, gen, genHW, acquiredAt, casRace>>

Next == \E n \in Nodes :
  \/ Tick(n)
  \/ Get(n)
  \/ Steal(n)
  \/ RenewLease(n)
  \/ Conflict(n)
  \/ Persist(n)
  \/ SelfFence(n)
  \/ Crash(n)
  \/ Recover(n)

Spec == Init /\ [][Next]_vars

==========================================================================
