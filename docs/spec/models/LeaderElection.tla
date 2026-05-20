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

\* Temporary -- Tasks 3 and 5 expand Next with the apiserver and fault actions.
Next == \E n \in Nodes : Tick(n)

Spec == Init /\ [][Next]_vars

==========================================================================
