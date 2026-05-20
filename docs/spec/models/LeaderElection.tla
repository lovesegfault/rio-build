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

EXTENDS Integers, FiniteSets

CONSTANTS Nodes,    \* set of node identities, e.g. {"a", "b", "c"}
          NULL      \* model value for "no holder"

VARIABLES lease,    \* [holder |-> Nodes \cup {NULL}, rv |-> Nat]
          state,    \* [Nodes -> {"Standby", "Leading"}]
          snap      \* [Nodes -> ([holder |-> ..., rv |-> ...] \cup {NULL})]
                    \* per-node GET snapshot -- what the node believes the
                    \* lease looks like. The CAS uses snap[n].rv as the
                    \* expected resourceVersion.

vars == <<lease, state, snap>>

\* -----------------------------------------------------------------------
\* Type invariant -- sanity check on the state space.
\* -----------------------------------------------------------------------

LeaseRecord == [holder : Nodes \cup {NULL}, rv : Nat]

TypeOK ==
  /\ lease \in LeaseRecord
  /\ state \in [Nodes -> {"Standby", "Leading"}]
  /\ snap \in [Nodes -> (LeaseRecord \cup {NULL})]

\* -----------------------------------------------------------------------
\* THE safety invariant -- at most one node believes it is leading.
\* The kube-leader-election 0.43 bug violates this: every CAS racer got
\* HTTP 200, every racer set its local state to Leading.
\* -----------------------------------------------------------------------

AtMostOneLeader == Cardinality({n \in Nodes : state[n] = "Leading"}) <= 1

\* -----------------------------------------------------------------------
\* Initial state: no lease, all standby.
\* -----------------------------------------------------------------------

Init ==
  /\ lease = [holder |-> NULL, rv |-> 0]
  /\ state = [n \in Nodes |-> "Standby"]
  /\ snap  = [n \in Nodes |-> NULL]

\* -----------------------------------------------------------------------
\* Actions
\* -----------------------------------------------------------------------

\* GET the lease -- atomic apiserver read. Snapshots the holder+rv.
Get(n) ==
  /\ snap[n] = NULL    \* don't re-GET an unspent snapshot
  /\ snap' = [snap EXCEPT ![n] = lease]
  /\ UNCHANGED <<lease, state>>

\* Replace the lease -- apiserver write, CAS-PRECONDITIONED on the
\* resourceVersion from the preceding GET. The apiserver returns 409
\* Conflict if the rv changed between GET and PUT. Exactly one of N
\* racing writers wins. This is the load-bearing line: delete the
\* `lease.rv = snap[n].rv` test and TLC reproduces the
\* kube-leader-election 0.43 bug (verified -- that's what the broken
\* model in this commit's first draft looked like).
Replace(n) ==
  IF lease.rv = snap[n].rv
  THEN \* CAS matched: write succeeds, apiserver bumps rv.
    /\ lease' = [holder |-> n, rv |-> lease.rv + 1]
    /\ state' = [state EXCEPT ![n] = "Leading"]
    /\ snap'  = [snap EXCEPT ![n] = NULL]
  ELSE \* CAS mismatch: 409 Conflict. Caller treats as not-leading
       \* (ElectionResult::Conflict in election.rs).
    /\ state' = [state EXCEPT ![n] = "Standby"]
    /\ snap'  = [snap EXCEPT ![n] = NULL]
    /\ UNCHANGED lease

\* The decide()->Steal path: node n GOT a snapshot, the snapshot says
\* the lease is empty, so steal it. (Spike scope: steal-only-when-empty.)
Steal(n) ==
  /\ state[n] = "Standby"
  /\ snap[n] /= NULL
  /\ snap[n].holder = NULL
  /\ Replace(n)

\* The decide()->Renew path: node n is leading and HAS a fresh GET
\* snapshot showing it as the holder. Renew = Replace at the snapshot's rv.
\* The CAS in Replace handles the "deposed since GET" case: if snap[n].rv
\* is stale, the ELSE branch fires and n steps down. (Unreachable in the
\* spike model -- no other node can ever pass the CAS once n is leading --
\* but the structure is what the Phase-1 model will exercise.)
Renew(n) ==
  /\ state[n] = "Leading"
  /\ snap[n] /= NULL
  /\ snap[n].holder = n
  /\ Replace(n)

\* The decide()->Standby path for a former leader: node n was Leading
\* but its GET shows someone else holds the lease. Step down. This is
\* the `was_leading && result != Leading` edge in election.rs's loop.
\* (Unreachable in the spike model for the same reason as Renew's 409
\* branch -- kept for case-structure parallelism with decide().)
Observe(n) ==
  /\ state[n] = "Leading"
  /\ snap[n] /= NULL
  /\ snap[n].holder /= n
  /\ state' = [state EXCEPT ![n] = "Standby"]
  /\ snap'  = [snap EXCEPT ![n] = NULL]
  /\ UNCHANGED lease

\* A standby node whose snapshot shows someone else holds the lease.
\* Spike scope: can't steal-when-non-empty (no time model). Clear the
\* snapshot so the node re-GETs next tick. Two reasons to keep it:
\*   (a) Fidelity: the implementation's try_acquire_or_renew() loop
\*       always GETs at the top of every tick, even when the node
\*       already knows it's standby. Discard models that per-tick
\*       re-GET cycle without modeling the observed-record clock.
\*   (b) Phase-1 forward-compat: once Renew's 409 branch becomes
\*       reachable in the full model, a deposed leader lands Standby
\*       with a stale non-NULL snap and would have no enabled action
\*       without Discard.
\* Note Discard is NOT load-bearing for the spike's deadlock check:
\* dropping it from Next leaves the same 524 distinct states with no
\* deadlock (verified). Next is `\E n \in Nodes`, and the leader
\* always has an enabled action (Get/Renew), so TLC's *global*
\* deadlock check never trips even when a per-node standby is stuck.
Discard(n) ==
  /\ state[n] = "Standby"
  /\ snap[n] /= NULL
  /\ snap[n].holder /= NULL
  /\ snap' = [snap EXCEPT ![n] = NULL]
  /\ UNCHANGED <<lease, state>>

Next ==
  \E n \in Nodes :
    \/ Get(n)
    \/ Steal(n)
    \/ Renew(n)
    \/ Observe(n)
    \/ Discard(n)

Spec == Init /\ [][Next]_vars

\* -----------------------------------------------------------------------
\* State-space bound -- TLC explores all reachable states; without this
\* the rv counter is unbounded and TLC never terminates.
\* -----------------------------------------------------------------------

MaxRv == 4

StateBound == lease.rv <= MaxRv

===============================================================================
