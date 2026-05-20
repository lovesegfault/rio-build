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
(*   - sched.lease.generation-claim    (the write-ahead claim in Steal;    *)
(*     its r[verify] marker lands with the DeleteLease extension, which    *)
(*     is the fault the claim exists to survive)                           *)
(*                                                                         *)
(* PROOF CLAIM, stated honestly: the invariants below hold under clock     *)
(* skew <= MaxSkew ticks, no host suspend, no Lease-object deletion, over  *)
(* a bounded state space of 2 nodes and ~2 leadership cycles (1.47M        *)
(* distinct states). The bounds exclude: 3-party races (any k-party CAS    *)
(* race contains a 2-party prefix, so AtMostOneCASWinner is unaffected; a  *)
(* 3-party dual-belief scenario needs two simultaneously-stale ex-leaders  *)
(* and is only partially covered), and a third leadership handoff. See     *)
(* the .cfg for the full coverage-vs-cost argument and the non-vacuity     *)
(* evidence.                                                               *)
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
(* sched.lease.generation-fence+2: when the dual-belief window is open,    *)
(* the executor must be able to tell the replicas apart by generation.     *)
(* HOLDS over the full state space now that the generation derives from    *)
(* the lease's transition count and is claimed in PG at acquisition time.  *)
(* The pre-fix protocol (in-memory fetch_add seeded from a lazily-written  *)
(* PG high-water mark) falsified it at depth 12; the weakened test         *)
(* (revert Steal's derivation, move the genHW update back to a             *)
(* dispatch-time Persist action) still reproduces that counterexample.     *)
(* See the FIXED block at the invariant definition.                        *)
(*                                                                         *)
(* ACTION <-> CODE CORRESPONDENCE (rio-lease/src/{lib,election}.rs):       *)
(*   Tick        clock advance; CLOCK_MONOTONIC ticks regardless of state  *)
(*   Get         try_acquire_or_renew()'s GET + decide()'s ObservedUpdate  *)
(*   Steal       decide()->Steal + replace(steal) + on_acquire(trans) +    *)
(*               seed_generation_from() + claim_generation() collapsed     *)
(*               into one atomic step (sound: recovery_complete gates      *)
(*               dispatch until the seed AND the claim have both run)      *)
(*   RenewLease  decide()->Renew + replace(renew) (named to avoid the      *)
(*               Renew constant); a same-epoch re-acquire retains its      *)
(*               generation and its claim row -- modeled by omission       *)
(*   Conflict    the 409 path; collapses 409-on-renew (lose) and           *)
(*               409-on-steal (never led) -- both reset the fence clock    *)
(*               and end Following                                         *)
(*   (Persist    REMOVED -- the write-ahead claim advances the PG floor    *)
(*               inside Steal; the lazy dispatch-time persist no longer    *)
(*               affects the floor abstraction. See the note at its old    *)
(*               definition site.)                                         *)
(*   SelfFence   maybe_self_fence(); may fire any time past the deadline   *)
(*               (a superset of the production every-RENEW_INTERVAL        *)
(*               schedule -- sound for safety)                             *)
(*   Crash/      pod restart: in-memory state lost, gen resets to the      *)
(*   Recover     AtomicU64::new(1) init; OS clock, apiserver, PG persist   *)
(*                                                                         *)
(* PHASE-2 DEFERRALS: liveness (eventually-some-leader needs an unbounded  *)
(* clock, which TLC cannot check -- the temporal form stutters one tick    *)
(* short of the fence deadline forever); NoDualDispatch (the worker-side   *)
(* model that takes StaleLeaderHasStaleGeneration as an ASSUME -- now      *)
(* unblocked); DeleteLease (the operator-resets-the-epoch-source fault     *)
(* that re-arms the transition-count derivation's failure mode and is      *)
(* closed by the write-ahead claim -- needs its own cfg and a fourth       *)
(* BoundedDualLeadership discovery disjunct); asymmetric TTLs (if the      *)
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
\* The bridge to sched.lease.generation-fence+2. When the dual-belief
\* window is open, the executor's generation fence has to be able to tell
\* the replicas apart -- the fresh leader's generation must be strictly
\* greater than the stale believer's. The production mechanism is
\* on_acquire(transitions)'s fetch_max(leaseTransitions + 1) plus the
\* write-ahead claim (sched.lease.generation-claim); the model encodes
\* both in Steal(n): gen[n]' = max(gen[n], lease.gen+2, genHW+1) and
\* genHW' = that same value.
\*
\* FIXED -- this invariant was FALSIFIED at depth 12 by the pre-fix
\* protocol (an in-memory fetch_add(1) seeded from a PG high-water mark
\* that only advanced at first dispatch). The counterexample: n1 steals
\* the empty lease (gen 2, seeded from genHW=0), is deposed before its
\* first dispatch-time PG write, n2 observes the staleness and steals,
\* seeding from the SAME genHW=0 -- both Leading at gen 2, and the
\* executor fence cannot tell their WorkAssignments apart. The window
\* between acquiring a generation and durably recording it was the hole.
\*
\* Two protocol changes closed it, and the model encodes both:
\*   (a) The generation derives from the lease's transition count
\*       (lease.gen+2 in Steal's target) -- the apiserver bumps
\*       leaseTransitions atomically with the holder change, so two
\*       distinct holders can never derive the same generation from it.
\*       In the counterexample trace, lease.gen was already 1 vs 2 while
\*       the in-memory generations collided at 2 vs 2.
\*   (b) The write-ahead claim (genHW' advances in Steal itself) -- the
\*       generation is durably recorded in PG before dispatch is ungated,
\*       so a successor's seed always exceeds every generation ever
\*       handed to dispatch, even one whose holder never dispatched
\*       anything.
\* Either change alone closes THIS counterexample; (b) is what survives
\* Lease-object deletion (which resets lease.gen to 0 and re-arms the (a)
\* mechanism's failure mode). Deletion is outside this model's fault set
\* -- there is no DeleteLease action -- and is addressed by a follow-up
\* model extension; the claim encoding it needs is already here.
\*
\* The invariant is ENABLED in the .cfg and holds over the full state
\* space. The deliberately-weakened test (run once during development,
\* NOT in CI): reverting Steal's derivation to the pre-fix
\* max(gen[n]+1, genHW+1) AND moving the genHW update back out of Steal
\* into a separate dispatch-time Persist action reproduces the depth-12
\* counterexample. That proves the encoding change is what makes the
\* invariant pass, not an accident of the surrounding edits.
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
\* (the deposed-leader path the spike could not reach). gen[n] and genHW are
\* set per on_acquire(transitions) + seed_generation_from() +
\* claim_generation() collapsed: all three run before recovery_complete=true
\* gates dispatch, so the model treats acquire/seed/claim as one atomic
\* step. r[sched.recovery.fetch-max-seed+2].
\*
\* The generation derives from the lease's POST-BUMP transition count: the
\* PUT writes lease.gen+1 (= leaseTransitions after the holder change), and
\* on_acquire() fetch_max'es (lease.gen+1)+1 = lease.gen+2 into the atomic
\* (r[sched.lease.generation-fence+2] -- the apiserver bumps the count
\* atomically with the holder change inside the rv-guarded PUT, so two
\* distinct holders can never derive the same value from it). The PG floor
\* contributes genHW+1 (seed_generation_from). Both are fetch_max against
\* the atomic's current value gen[n] -- nothing unconditionally increments.
\*
\* genHW' is the write-ahead claim (sched.lease.generation-claim): the
\* generation is durably recorded in PG's claims ledger before dispatch is
\* ungated, so the floor advances at acquisition time. There is no
\* acquire-to-persist window for a deposed leader to die inside -- the
\* window the pre-fix counterexample exploited (see the FIXED note at
\* StaleLeaderHasStaleGeneration).
Steal(n) ==
  LET target == IF lease.gen + 2 > genHW + 1 THEN lease.gen + 2 ELSE genHW + 1
      seeded == IF gen[n] > target THEN gen[n] ELSE target IN
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
  \* The claim: fetch_max in shape, though seeded >= genHW+1 always holds
  \* here (target's ELSE arm is genHW+1 and seeded >= target).
  /\ genHW' = IF seeded > genHW THEN seeded ELSE genHW
  /\ acquiredAt' = [acquiredAt EXCEPT ![n] = snap[n].rv]
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ obs'   = [obs   EXCEPT ![n] = NULL]      \* on_acquire clears observed
  /\ snap'  = [snap  EXCEPT ![n] = NULL]      \* spent the snapshot
  /\ UNCHANGED <<clocks, alive>>

\* The decide()->Renew path: we hold the lease, refresh renew_time. Bumps rv
\* (the apiserver bumps on every write); does NOT touch holder/gen.
\* Named RenewLease (not Renew) -- the constant Renew is RENEW_INTERVAL.
\*
\* The idempotent self re-claim (sched.lease.generation-claim's "retain own
\* epoch" clause) maps HERE, by omission: a self-fence false alarm followed
\* by a successful renew is a re-acquisition of the SAME epoch (the holder
\* did not change, leaseTransitions did not move), and the code's claim
\* path finds its own row at its own generation and retains it. The model
\* encodes that as RenewLease leaving gen and genHW unchanged. The
\* "bump past a foreign claim" branch is Steal's genHW+1 arm. Known
\* under-approximation: RenewLease does not restore state[n] to "Leading"
\* after a SelfFence false alarm (production's acquire edge does). That
\* path requires lease.holder = n, which precludes any other node holding
\* the lease -- so the states it would add are sole-leader states already
\* reachable via Steal, and no dual-belief state (the only states the
\* generation invariant constrains) is missed.
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

\* Persist(n) -- REMOVED. It modeled the lazy dispatch-time PG write of the
\* new generation (genHW' = gen[n] once the leader started dispatching);
\* the acquire-to-Persist window was exactly what the pre-fix
\* counterexample exploited. The write-ahead claim moves the genHW update
\* into Steal(n) itself, which makes Persist's precondition
\* (gen[n] > genHW for an alive Leading node) unreachable: every Steal
\* establishes gen[n] = genHW = seeded, and only another node's later
\* Steal can change either. A never-enabled action in Next is misleading
\* -- it would suggest the persist window still exists -- so the action is
\* gone rather than kept as a no-op. Production still writes assignment
\* rows at dispatch time, but that write no longer affects the PG floor
\* abstraction (the floor is GREATEST(assignments, claims) and the claims
\* arm already holds every generation ever acquired).

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
  \/ SelfFence(n)
  \/ Crash(n)
  \/ Recover(n)

Spec == Init /\ [][Next]_vars

==========================================================================
