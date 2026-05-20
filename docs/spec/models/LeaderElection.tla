---------------------------- MODULE LeaderElection ----------------------------
(***************************************************************************)
(* Phase-1 model of rio-lease's apiserver-CAS leader election protocol:    *)
(* per-node monotonic clocks with bounded skew, the observed-record        *)
(* staleness clock, local self-fencing, the PG generation high-water       *)
(* mark, and pod crash/recovery.                                           *)
(*                                                                         *)
(* Formalizes (from docs/spec/components/scheduler.typ; update both when   *)
(* the protocol changes -- `tracey bump` on a rule flags the r[verify]     *)
(* marker in nix/tla.nix as stale):                                        *)
(*   - sched.lease.at-most-one-leader  (the safety invariant, both halves) *)
(*   - sched.lease.k8s-lease           (the protocol mechanism)            *)
(*   - sched.recovery.fetch-max-seed   (the generation seeding on acquire) *)
(*                                                                         *)
(* PROOF CLAIM, stated honestly: the invariants below hold under clock     *)
(* skew <= MaxSkew ticks, no host suspend, over a bounded state space of   *)
(* 2 nodes and ~2 leadership cycles (4.27M distinct states). The bounds    *)
(* exclude: 3-party races (any k-party CAS race contains a 2-party prefix, *)
(* so AtMostOneCASWinner is unaffected; a 3-party dual-belief scenario     *)
(* needs two simultaneously-stale ex-leaders and is only partially         *)
(* covered), and a third leadership handoff. See the .cfg for the full     *)
(* coverage-vs-cost argument and the non-vacuity evidence.                 *)
(*                                                                         *)
(* INVARIANTS, and which half of sched.lease.at-most-one-leader+2 each     *)
(* verifies:                                                               *)
(*                                                                         *)
(* AtMostOneCASWinner -- the hard half: the apiserver's optimistic         *)
(* concurrency admits at most one writer per resourceVersion. Weakened     *)
(* test: deleting the CAS precondition from ReplaceGuard reproduces the    *)
(* kube-leader-election 0.43 bug at depth 6 (both racers GET rv=0, both    *)
(* PUT, both Leading -- and both reach gen=2, so the generation fence      *)
(* cannot tell them apart either).                                         *)
(*                                                                         *)
(* BoundedDualLeadership -- the soft half: two replicas CAN concurrently   *)
(* believe they lead (a deposed leader that has not yet noticed), but      *)
(* every reachable dual-belief state already has a discovery mechanism     *)
(* armed -- the stale believer is within 2*MaxSkew of its own self-fence   *)
(* deadline, or its snapshot is stale (next renew 409s), or its snapshot   *)
(* already names the thief. Combined with LoopInterval (the loop runs      *)
(* every Renew ticks), the window is bounded to one loop iteration plus    *)
(* the skew penalty. Weakened test: relaxing Steal's staleness threshold   *)
(* from > Ttl to >= 0 violates it at depth 5 -- the > Ttl threshold is     *)
(* exactly what guarantees the victim is already at its own fence          *)
(* deadline when deposed. Clock skew eats directly into that guarantee     *)
(* (the 2*MaxSkew term); this is the formal version of the asymmetric-TTL  *)
(* TODO above LEASE_TTL in rio-lease/src/lib.rs.                           *)
(*                                                                         *)
(* StaleLeaderHasStaleGeneration -- the bridge to                          *)
(* sched.lease.generation-fence: when the dual-belief window is open, the  *)
(* executor must be able to tell the replicas apart by generation.         *)
(* FALSIFIED at depth 12; committed disabled in the .cfg. See the KNOWN    *)
(* COUNTEREXAMPLE block at the invariant definition for the trace, the     *)
(* protocol-level diagnosis (the acquire-to-Persist window leaves PG's     *)
(* high-water stale), and the candidate fixes.                             *)
(*                                                                         *)
(* ACTION <-> CODE CORRESPONDENCE (rio-lease/src/{lib,election}.rs):       *)
(*   Tick        clock advance; CLOCK_MONOTONIC ticks regardless of state  *)
(*   Get         try_acquire_or_renew()'s GET + decide()'s ObservedUpdate  *)
(*   Steal       decide()->Steal + replace(steal) + on_acquire() +         *)
(*               seed_generation_from() collapsed into one atomic step     *)
(*               (sound: recovery_complete gates dispatch until the seed)  *)
(*   RenewLease  decide()->Renew + replace(renew) (named to avoid the      *)
(*               Renew constant)                                           *)
(*   Conflict    the 409 path; collapses 409-on-renew (lose) and           *)
(*               409-on-steal (never led) -- both reset the fence clock    *)
(*               and end Following                                         *)
(*   Persist     the first dispatch-time PG write of the new generation    *)
(*   SelfFence   maybe_self_fence(); may fire any time past the deadline   *)
(*               (a superset of the production every-RENEW_INTERVAL        *)
(*               schedule -- sound for safety)                             *)
(*   Crash/      pod restart: in-memory state lost, gen resets to the      *)
(*   Recover     AtomicU64::new(1) init; OS clock, apiserver, PG persist   *)
(*                                                                         *)
(* PHASE-2 DEFERRALS: liveness (eventually-some-leader needs an unbounded  *)
(* clock, which TLC cannot check -- the temporal form stutters one tick    *)
(* short of the fence deadline forever); NoDualDispatch (the worker-side   *)
(* model that takes StaleLeaderHasStaleGeneration as an ASSUME -- blocked  *)
(* on fixing the generation collision first); asymmetric TTLs (if the      *)
(* lib.rs TODO lands, StealTtl splits from FenceTtl and the 2*MaxSkew      *)
(* penalty in BoundedDualLeadership shrinks).                              *)
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
\* either. Recorded in the header.
\* -----------------------------------------------------------------------
AtMostOneCASWinner == ~casRace

\* -----------------------------------------------------------------------
\* The soft half of sched.lease.at-most-one-leader+2. Two nodes
\* simultaneously believing they lead IS reachable (the dual-belief
\* window: a deposed leader that hasn't yet noticed). The property is
\* that the window is BOUNDED to one loop iteration. That claim splits
\* into an assumption and a theorem:
\*
\*   ASSUMPTION (LoopInterval, enforced by Tick's precondition): the
\*   loop runs every Renew ticks, so a Leading node takes its next
\*   GET / PUT / maybe_self_fence within Renew ticks of any state.
\*
\*   THEOREM (BoundedDualLeadership): in every dual-belief state, the
\*   stale believer's next loop iteration DISCOVERS the loss -- one of
\*   its three discovery paths is already armed. The window is therefore
\*   <= one loop iteration. The paths:
\*     (1) Self-fence: clocks[stale] - fence[stale] > Ttl - 2*MaxSkew.
\*         The thief measured > Ttl of staleness on ITS clock before
\*         stealing; the victim's own fence age is within 2*MaxSkew of
\*         that, so the victim is at (or within 2*MaxSkew ticks of) its
\*         own self-fence deadline. The 2*MaxSkew correction is the
\*         price of per-node clocks -- with a shared clock the bound is
\*         exactly Ttl. With MaxSkew >= Ttl/2 the disjunct is vacuous
\*         (clocks - fence >= 0 always) and the protocol gives no fence
\*         guarantee -- the steal threshold and the fence threshold are
\*         only coupled when skew is small relative to Ttl.
\*     (2) 409: snap[stale].rv /= lease.rv -- the snapshot in hand is
\*         stale, the next PUT gets 409 Conflict, the loop flips to
\*         Following (the lose transition).
\*     (3) Observation: snap[stale].holder /= stale -- the snapshot
\*         already shows the new holder; the next decide() returns
\*         Standby and the loop flips to Following.
\*   Path (1) covers the partitioned victim (no successful round-trips,
\*   fence aging). Paths (2)/(3) cover the connected victim (its GETs
\*   keep refreshing fence, but every refresh hands it a snapshot that
\*   exposes the loss). A deposed believer with a fresh fence and NO
\*   snapshot is unreachable: the only fence-refreshing actions either
\*   produce a snapshot (Get) or require holding the lease (RenewLease).
\*
\* Why a state invariant and not the temporal [](Dual => <>~Dual) with
\* WF on SelfFence: the temporal form is unsound under a bounded,
\* fairness-free clock. TLC finds a stuttering counterexample in which
\* the stale believer's clock simply stops one tick short of its fence
\* deadline -- SelfFence is never *enabled*, so weak fairness never
\* forces it. Adding WF on Tick does not fix this: Tick is disabled at
\* the MaxTime ceiling, so a Dual window that opens within Ttl of the
\* ceiling can never accumulate enough ticks to reach the deadline.
\* "Eventually" is the wrong claim for a bounded model; "the discovery
\* path is already armed" is the right claim, and it is stronger.
\*
\* Deliberately-weakened test (run once during development, NOT in CI):
\* weakening Steal's staleness guard from `> Ttl` to `>= 0` (steal on
\* first observation) produces a counterexample at depth 5: n1 acquires,
\* n2 observes and steals immediately, n1 is deposed with a fresh fence
\* (clocks - fence = 0) and no snapshot -- no discovery path is armed.
\* The invariant is what couples the steal threshold to the fence
\* threshold: a thief may only steal a lease whose holder is already at
\* (or within 2*MaxSkew of) its own self-fence deadline.
\* -----------------------------------------------------------------------
Dual == \E m, n \in Nodes : m /= n /\ state[m] = "Leading" /\ state[n] = "Leading"

\* The production loop's tick discipline, stated as a predicate over
\* states: a Leading node is never more than Ttl + Renew past its last
\* successful round-trip, because run_lease_loop() checks
\* maybe_self_fence() every RENEW_INTERVAL and cannot skip the check.
\* This is ENFORCED by Tick(n)'s precondition (see the comment there for
\* why a precondition and not a state CONSTRAINT); it is listed as an
\* INVARIANT in the .cfg purely as a tripwire -- if a future edit to
\* Tick(n) drops the precondition, LoopInterval fails loudly instead of
\* BoundedDualLeadership failing confusingly one tick past the bound.
LoopInterval ==
  \A n \in Nodes :
    (alive[n] /\ state[n] = "Leading") => clocks[n] - fence[n] <= Ttl + Renew

BoundedDualLeadership ==
  \A m, n \in Nodes :
    (state[m] = "Leading" /\ state[n] = "Leading" /\ m /= n) =>
      /\ lease.holder \in {m, n}
      /\ LET stale == IF lease.holder = m THEN n ELSE m
         IN \/ clocks[stale] - fence[stale] > Ttl - 2 * MaxSkew
            \/ /\ snap[stale] /= NULL
               /\ \/ snap[stale].rv /= lease.rv
                  \/ snap[stale].holder /= stale

\* -----------------------------------------------------------------------
\* The bridge to sched.lease.generation-fence. When the dual-belief window
\* is open, the executor's generation fence has to be able to tell the
\* replicas apart -- the fresh leader's generation must be strictly
\* greater than the stale believer's. The production mechanism is
\* on_acquire()'s fetch_add(1) seeded from PG's generation high-water mark
\* (seed_generation_from(), r[sched.recovery.fetch-max-seed]); the model
\* collapses both into Steal(n)'s max(gen[n]+1, genHW+1).
\*
\* KNOWN COUNTEREXAMPLE -- this invariant is FALSIFIED by the model at
\* depth 12 (disabled in the .cfg so CI stays green; the definition is
\* kept so the fix can re-enable it). The trace, in protocol terms:
\*
\*   1-3.  n1 GETs the empty lease (rv=0).
\*   4.    n1 Steals: gen[n1] = max(1+1, 0+1) = 2, lease = (n1, rv=1,
\*         lease.gen=1), genHW STAYS 0 -- n1 has not yet persisted.
\*   5.    n2 GETs, sees (n1, rv=1), starts observing rv=1 at its clock.
\*   6-11. Clocks tick. n1 never Persists (in production: the leader is
\*         still inside recovery, or has no dispatch traffic -- anything
\*         that delays the first PG write of the new generation). n1
\*         never RenewLeases either (partitioned from the apiserver
\*         after the acquire: its snapshot was spent by the Steal and it
\*         never completes another GET). n1's rv=1 therefore never
\*         changes.
\*   12.   n2's observed rv=1 has been stale for 4 > Ttl=3 on its clock
\*         -> Steal: gen[n2] = max(1+1, 0+1) = 2 -- genHW is STILL 0, so
\*         n2 seeds from the same high-water n1 seeded from. Both n1 and
\*         n2 are Leading with gen = 2. The generation fence cannot tell
\*         them apart.
\*
\* Protocol-level diagnosis: the window between acquire (seed the
\* in-memory generation from PG's high-water mark) and the first Persist
\* (write the new generation back to PG) is real. A leader deposed inside
\* that window leaves genHW unchanged, and the thief seeds from the same
\* value. r[sched.recovery.fetch-max-seed] protects against a CRASHED
\* leader reusing an old generation; it does not protect against a
\* DEPOSED-BEFORE-PERSISTING leader colliding with its successor.
\*
\* What this means for r[sched.lease.generation-fence]: in this
\* interleaving the executor cannot distinguish the stale leader's
\* WorkAssignments from the fresh leader's by generation alone. The
\* existing fallback argument in that rule's text (the PG writes are
\* idempotent upserts keyed by drv_hash with monotone status transitions)
\* still applies -- the collision makes the fence ineffective, not the
\* system incorrect. But the fence is the *stated* mechanism for
\* executor-side staleness detection, and this shows it has a hole.
\*
\* Candidate fixes (a protocol design decision, not a model decision):
\*   (a) Derive the generation from the lease's lease_transitions: the
\*       apiserver bumps it atomically with the holder change, so there
\*       is no persist window at all. (The model's lease.gen already
\*       tracks this -- note lease.gen IS distinct across the two
\*       acquisitions in the trace above: n1 acquired at lease.gen=1,
\*       n2 at lease.gen=2.)
\*   (b) Make the generation persist part of the acquire critical
\*       section: block dispatch until the new generation is durably in
\*       PG. recovery_complete almost does this, but the persist has to
\*       happen-before the first generation-stamped WorkAssignment, not
\*       just before recovery completes.
\*   (c) Accept the gap: rely on the idempotent-PG-writes argument.
\* -----------------------------------------------------------------------
StaleLeaderHasStaleGeneration ==
  \A m, n \in Nodes :
    (state[m] = "Leading" /\ state[n] = "Leading" /\ m /= n) =>
      LET stale == IF lease.holder = m THEN n ELSE m
          fresh == IF lease.holder = m THEN m ELSE n
      IN gen[stale] < gen[fresh]

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
\* Time. clocks[n] advances independently. Ticks regardless of process
\* state (alive or not) -- CLOCK_MONOTONIC does not stop for a dead pod.
\* Bounded skew is a state CONSTRAINT, not a Tick precondition: it bounds
\* how far one node's clock can lag another's, not when Tick fires.
\*
\* The second conjunct encodes the production loop's tick discipline (the
\* LoopInterval assumption): run_lease_loop() calls maybe_self_fence()
\* every RENEW_INTERVAL, so a Leading node's clock cannot advance more
\* than Ttl + Renew past its last successful round-trip -- the loop would
\* have fenced it first. Encoding this as a Tick precondition (the state
\* is never generated) rather than a state CONSTRAINT (the state is
\* generated, checked against invariants, and only then discarded) keeps
\* the production-unreachable states out of invariant checking: TLC
\* checks invariants on constraint-violating states before pruning them,
\* which would fail BoundedDualLeadership one tick past the bound.
\* -----------------------------------------------------------------------
Tick(n) ==
  /\ clocks[n] < MaxTime
  /\ (alive[n] /\ state[n] = "Leading") =>
       (clocks[n] + 1) - fence[n] <= Ttl + Renew
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

\* The shared CAS guard for Steal/RenewLease. NOT a standalone action -- a
\* helper that Steal and RenewLease conjoin. The casRace history is
\* recorded HERE: it's the only place a CAS conflict can be observed.
\* casRace flips when this node's PUT succeeds at an rv that another
\* currently-Leading node also acquired at -- the apiserver let two
\* writers through.
ReplaceGuard(n) ==
  /\ snap[n] /= NULL
  /\ snap[n].rv < MaxRv
  \* THE CAS PRECONDITION. The deliberately-weakened CAS test documented
  \* in the header deletes the next line.
  /\ lease.rv = snap[n].rv
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
