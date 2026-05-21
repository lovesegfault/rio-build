---------------------------- MODULE LeaderElection ----------------------------
(***************************************************************************)
(* Phase-1 model of rio-lease's apiserver-CAS leader election protocol:    *)
(* per-node monotonic clocks with bounded skew, the observed-record        *)
(* staleness clock, local self-fencing, the PG generation high-water       *)
(* mark, pod crash/recovery, and Lease-object deletion.                    *)
(*                                                                         *)
(* Formalizes (from docs/spec/components/scheduler.typ; update both when   *)
(* the protocol changes -- `tracey bump` on a rule flags the r[verify]     *)
(* marker in nix/tla.nix as stale):                                        *)
(*   - sched.lease.at-most-one-leader  (the safety invariant, both halves) *)
(*   - sched.lease.k8s-lease           (the protocol mechanism)            *)
(*   - sched.recovery.fetch-max-seed   (the generation seeding on acquire) *)
(*   - sched.lease.generation-claim    (the write-ahead claim in Steal;    *)
(*     verified by the DeleteLease cfg, which injects the fault the claim  *)
(*     exists to survive)                                                  *)
(*                                                                         *)
(* PROOF CLAIM, stated honestly: the invariants below hold under clock     *)
(* skew <= MaxSkew ticks, no host suspend, over a bounded state space of   *)
(* 2 nodes and ~2 leadership cycles, with at most MaxDeletes Lease-object  *)
(* deletions (LeaderElection.cfg pins MaxDeletes=0, ~1.61M distinct        *)
(* states; LeaderElectionDeletion.cfg pins MaxDeletes=1 -- same .tla, the  *)
(* deletion fault enabled). The bounds exclude: 3-party races (any         *)
(* k-party CAS race contains a 2-party prefix, so AtMostOneCASWinner is    *)
(* unaffected; a 3-party dual-belief scenario needs two                    *)
(* simultaneously-stale ex-leaders and is only partially covered), a       *)
(* third leadership handoff, and a second deletion. See the .cfgs for the  *)
(* full coverage-vs-cost argument and the non-vacuity evidence.            *)
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
(* already names the thief, or it was deposed by a Lease deletion (then    *)
(* the bound degrades to its own self-fence deadline, FenceAfter + Renew,  *)
(* since                                                                   *)
(* no thief ever measured its staleness). Combined with LoopInterval (the  *)
(* loop runs every Renew ticks), the window is bounded to one loop         *)
(* iteration plus the skew penalty (or one lease lifetime for a deletion   *)
(* victim). Weakened test: relaxing Steal's staleness threshold from       *)
(* > StealAfter to >= 0 violates it (BFS terminates at depth 12) -- the    *)
(* > StealAfter threshold is exactly what guarantees the victim is         *)
(* already at (or within 2*MaxSkew - separation of) its own fence          *)
(* deadline when deposed. Clock skew eats directly into that guarantee     *)
(* and the fence/steal separation buys it back: at separation >=           *)
(* Renew + 2*MaxSkew the victim has ALREADY fenced (or its clock has       *)
(* stopped at the LoopInterval cap) when the thief is first allowed to     *)
(* steal, and NeverDual (below) holds instead. See the TWO OPERATING       *)
(* REGIMES note at the end of this header.                                 *)
(*                                                                         *)
(* StaleLeaderHasStaleGeneration -- the bridge to                          *)
(* sched.lease.generation-fence+2: when the dual-belief window is open,    *)
(* the executor must be able to tell the replicas apart by generation      *)
(* (the holder's strictly greatest when a holder exists among the          *)
(* believers; merely distinct when a deletion has deposed both). HOLDS     *)
(* over the full state space of BOTH cfgs now that the generation          *)
(* derives from the lease's transition count and is claimed in PG at       *)
(* acquisition time. The pre-fix protocol (in-memory fetch_add seeded      *)
(* from a lazily-written PG high-water mark) falsified it at depth 12      *)
(* with no deletion and at depth 6 with one; the weakened test (move the   *)
(* genHW update back to a dispatch-time Persist action) still reproduces   *)
(* both counterexamples. See the FIXED block at the invariant definition   *)
(* and the non-vacuity section of LeaderElectionDeletion.cfg.              *)
(*                                                                         *)
(* ACTION <-> CODE CORRESPONDENCE (rio-lease/src/{lib,election}.rs):       *)
(*   Tick        clock advance; CLOCK_MONOTONIC ticks regardless of state  *)
(*   Get         try_acquire_or_renew()'s GET + decide()'s ObservedUpdate  *)
(*   Steal       decide()->Steal + replace(steal) + on_acquire(trans) +    *)
(*               seed_generation_from() + claim_generation() collapsed     *)
(*               into one atomic step (sound: recovery_complete gates      *)
(*               dispatch until the seed AND the claim have both run)      *)
(*   RenewLease  decide()->Renew + replace(renew) (named to avoid the      *)
(*               Renew constant) + the acquire arm's on_acquire(trans)     *)
(*               with the UN-bumped count: restores Leading after a        *)
(*               self-fence false alarm or a crash at the retained (or     *)
(*               crash-restored) generation -- the same-epoch re-acquire   *)
(*               and the idempotent self re-claim                          *)
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
(*   DeleteLease `kubectl delete lease` + the next replica's create():     *)
(*               holder and leaseTransitions reset, resourceVersion does   *)
(*               not, PG survives. Disabled (MaxDeletes=0) in the base     *)
(*               cfg; LeaderElectionDeletion.cfg enables one per trace.    *)
(*                                                                         *)
(* PHASE-2 DEFERRALS: liveness (eventually-some-leader needs an unbounded  *)
(* clock, which TLC cannot check -- the temporal form stutters one tick    *)
(* short of the fence deadline forever); NoDualDispatch (the worker-side   *)
(* model that takes StaleLeaderHasStaleGeneration as an ASSUME -- now      *)
(* unblocked); a second deletion per trace (MaxDeletes=2 would cover an    *)
(* operator deleting the lease twice before the first victim's window      *)
(* closes -- the claim argument is inductive in genHW so nothing new is    *)
(* expected, but it is unverified).                                        *)
(*                                                                         *)
(* TWO OPERATING REGIMES (the asymmetric-TTL split): FenceAfter and        *)
(* StealAfter are separate constants. A Leading victim's measured          *)
(* staleness on a thief's clock is at most FenceAfter + Renew + 2*MaxSkew  *)
(* (the LoopInterval Tick cap plus the round-trip skew), so:               *)
(* When StealAfter - FenceAfter < Renew + 2*MaxSkew (the base and          *)
(* deletion cfgs: zero separation), a thief can clear its steal threshold  *)
(* while the victim still believes -- dual belief is reachable and         *)
(* BoundedDualLeadership is the operative property. When StealAfter -      *)
(* FenceAfter >= Renew + 2*MaxSkew (LeaderElectionAsymmetric.cfg,          *)
(* separation 3 = 1 + 2 with equality), the steal threshold is             *)
(* unreachable while the victim still believes and NeverDual holds: no     *)
(* dual-belief state exists at all. Both boundaries are measured: the      *)
(* separation-2 run violates NeverDual, the separation-3 run proves it.    *)
(***************************************************************************)

EXTENDS Integers, FiniteSets, TLC

CONSTANTS Nodes,    \* set of node identities, e.g. {n1, n2, n3}
          NULL,     \* model value for "no holder" / "no snapshot"
          FenceAfter, \* SELF_FENCE_AFTER in ticks: the leader self-fences
                    \* after this long without a successful round-trip.
                    \* "The leader deciding it no longer leads."
          StealAfter, \* STEAL_AFTER in ticks: a follower steals after
                    \* observing the same rv for this long. "The follower
                    \* deciding the leader is dead." The two were a single
                    \* Ttl = LEASE_TTL before the asymmetric-TTL split;
                    \* StealAfter - FenceAfter is the safety margin that
                    \* decides which regime a cfg models (see NeverDual).
          Renew,    \* RENEW_INTERVAL in ticks (FenceAfter/Renew ~= 2-3)
          MaxSkew,  \* clock skew bound
          MaxTime,  \* clock ceiling (state-space bound)
          MaxGen,   \* generation ceiling (state-space bound)
          MaxRv,    \* rv ceiling (state-space bound)
          MaxDeletes \* Lease-object deletions per trace (0 = fault disabled)

ASSUME FenceAfter \in Nat /\ StealAfter \in Nat /\ Renew \in Nat
       /\ Renew >= 1 /\ FenceAfter >= Renew /\ StealAfter >= FenceAfter
ASSUME MaxSkew \in Nat /\ MaxTime \in Nat /\ MaxGen \in Nat /\ MaxRv \in Nat
ASSUME MaxDeletes \in Nat

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
  casRace,     \* BOOLEAN -- has any CAS race been observed?
  deletes,     \* 0..MaxDeletes -- Lease-object deletions so far
  delVictims   \* SUBSET Nodes -- holders deposed by a deletion, not a steal

vars == <<clocks, lease, alive, state, snap, obs, fence, gen, genHW, acquiredAt,
          casRace, deletes, delVictims>>

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
  /\ deletes \in 0..MaxDeletes
  /\ delVictims \in SUBSET Nodes

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
\*     (1) Self-fence: clocks[stale] - fence[stale] > StealAfter - 2*MaxSkew.
\*         The thief measured > StealAfter of staleness on ITS clock
\*         before stealing; the victim's own fence age is within
\*         2*MaxSkew of that, so the victim's fence age exceeds
\*         StealAfter - 2*MaxSkew on its own clock. It self-fences at
\*         FenceAfter. The distance to its deadline is therefore at most
\*         FenceAfter - (StealAfter - 2*MaxSkew) = 2*MaxSkew - separation.
\*         With zero separation (the base/deletion cfgs) that is 2*MaxSkew
\*         -- the victim is within 2*MaxSkew ticks of fencing. With
\*         separation >= Renew + 2*MaxSkew (the asymmetric cfg) the
\*         victim is at least Renew ticks PAST its deadline -- beyond the
\*         LoopInterval slack, so the Tick cap has already forced its
\*         fence to fire or its clock to stop before the thief's steal
\*         threshold clears. That is why NeverDual holds there and this
\*         disjunct never needs to fire. The 2*MaxSkew
\*         correction is the price of per-node clocks -- with a shared
\*         clock the victim's fence age is exactly what the thief
\*         measured. With MaxSkew >= StealAfter/2 the disjunct is vacuous
\*         (clocks - fence >= 0 always) and the protocol gives no fence
\*         guarantee -- the steal threshold and the fence threshold are
\*         only coupled when skew is small relative to StealAfter.
\*     (2) 409: snap[stale].rv /= lease.rv -- the snapshot in hand is
\*         stale, the next PUT gets 409 Conflict, the loop flips to
\*         Following (the lose transition).
\*     (3) Observation: snap[stale].holder /= stale -- the snapshot
\*         already shows the new holder; the next decide() returns
\*         Standby and the loop flips to Following.
\*   Path (1) covers the blind victim (no completed write since its
\*   acquisition, fence aging toward the deadline). Paths (2)/(3) cover
\*   the connected victim (its GETs hand it a snapshot that exposes the
\*   loss without touching its fence). A deposed believer's fence NEVER
\*   refreshes: the only fence-resetting actions are a successful write
\*   (Steal/RenewLease -- requires winning the CAS, which a deposed
\*   believer's stale snapshot cannot) or a 409 (Conflict -- which makes
\*   it Following, no longer a believer). Its fence is frozen at its last
\*   successful write and only ages.
\*
\* Why a state invariant and not the temporal [](Dual => <>~Dual) with
\* WF on SelfFence: the temporal form is unsound under a bounded,
\* fairness-free clock. TLC finds a stuttering counterexample in which
\* the stale believer's clock simply stops one tick short of its fence
\* deadline -- SelfFence is never *enabled*, so weak fairness never
\* forces it. Adding WF on Tick does not fix this: Tick is disabled at
\* the MaxTime ceiling, so a Dual window that opens within FenceAfter of
\* the ceiling can never accumulate enough ticks to reach the deadline.
\* "Eventually" is the wrong claim for a bounded model; "the discovery
\* path is already armed" is the right claim, and it is stronger.
\*
\* Deliberately-weakened test (run once during development, NOT in CI):
\* weakening Steal's staleness guard from `> StealAfter` to `>= 0` (steal
\* on first observation) produces a counterexample at depth 5: n1
\* acquires, n2 observes and steals immediately, n1 is deposed with a
\* fresh fence (clocks - fence = 0) and no snapshot -- no discovery path
\* is armed. The invariant is what couples the steal threshold to the
\* fence threshold: a thief may only steal a lease whose holder is
\* already at (or within 2*MaxSkew - (StealAfter - FenceAfter) of) its
\* own self-fence deadline.
\* -----------------------------------------------------------------------
Dual == \E m, n \in Nodes : m /= n /\ state[m] = "Leading" /\ state[n] = "Leading"

\* The healthy-regime invariant: no two replicas ever simultaneously
\* believe they lead. Holds iff the fence/steal separation covers both
\* the LoopInterval slack and the round-trip clock skew:
\*   StealAfter - FenceAfter >= Renew + 2*MaxSkew.
\* A thief's measured staleness is anchored to the victim's last
\* completed write (the rv change); the victim's fence is anchored to the
\* same event (a bare read does not move it -- see Get(n)); the victim's
\* clock cannot advance more than FenceAfter + Renew past that anchor
\* while it still believes (the LoopInterval Tick cap); the thief's clock
\* is within MaxSkew of the victim's and its observation anchor is within
\* MaxSkew the other way. So the thief's measured staleness while the
\* victim still believes is at most FenceAfter + Renew + 2*MaxSkew, and a
\* steal threshold at or above that is unreachable until the victim has
\* stopped believing. Checked in LeaderElectionAsymmetric.cfg (separation
\* 3 >= 1 + 2, holds; separation 2, violated -- the boundary is measured
\* from both sides). Deliberately VIOLATED in the base and deletion cfgs
\* (zero separation), where it doubles as the non-vacuity probe for
\* BoundedDualLeadership: dual belief is reachable there, and every
\* instance has a discovery mechanism armed.
NeverDual == ~Dual

\* The production loop's tick discipline, stated as a predicate over
\* states: a Leading node is never more than FenceAfter + Renew past its
\* last successful round-trip, because run_lease_loop() checks
\* maybe_self_fence() every RENEW_INTERVAL and cannot skip the check.
\* This is ENFORCED by Tick(n)'s precondition (see the comment there for
\* why a precondition and not a state CONSTRAINT); it is listed as an
\* INVARIANT in the .cfg purely as a tripwire -- if a future edit to
\* Tick(n) drops the precondition, LoopInterval fails loudly instead of
\* BoundedDualLeadership failing confusingly one tick past the bound.
LoopInterval ==
  \A n \in Nodes :
    (alive[n] /\ state[n] = "Leading") => clocks[n] - fence[n] <= FenceAfter + Renew

BoundedDualLeadership ==
  \A m, n \in Nodes :
    (state[m] = "Leading" /\ state[n] = "Leading" /\ m /= n) =>
      \* In every dual one of the believers actually holds the lease --
      \* unless an operator deleted it out from under the holder while a
      \* previously-deposed believer still hadn't noticed ITS loss; then
      \* neither does and BOTH believers are stale. With no deletion the
      \* holder is always the most recent stealer, which is Leading (or
      \* crashed, and a crashed node is not a believer).
      /\ \/ lease.holder \in {m, n}
         \/ lease.holder = NULL /\ deletes > 0
      \* Every stale believer -- the believers that do NOT hold the
      \* lease (one of the two normally; both after a deletion) -- has a
      \* discovery mechanism armed:
      /\ \A stale \in {m, n} \ {lease.holder} :
           \/ clocks[stale] - fence[stale] > StealAfter - 2 * MaxSkew
           \/ /\ snap[stale] /= NULL
              /\ \/ snap[stale].rv /= lease.rv
                 \/ snap[stale].holder /= stale
           \* (4) Deposed by an operator deleting the Lease out from
           \*     under it (DeleteLease) rather than by a thief that
           \*     observed it as stale. The steal-threshold coupling of
           \*     disjunct (1) does not apply -- no thief ever measured
           \*     this victim's staleness, so its fence can be arbitrarily
           \*     fresh -- and its snapshot was spent at its own
           \*     acquisition, so (2)/(3) have nothing to look at. Its
           \*     window is bounded by its OWN self-fence deadline
           \*     instead: LoopInterval caps a Leading node at
           \*     FenceAfter + Renew ticks past its last round-trip and
           \*     SelfFence is enabled from FenceAfter + 1, so the window
           \*     is at most FenceAfter + Renew (one full fence lifetime)
           \*     rather than 2*MaxSkew + Renew. Weaker, but still
           \*     finite, and the
           \*     generation fence (StaleLeaderHasStaleGeneration) is
           \*     what protects correctness inside the window. A victim
           \*     that reaches the apiserver before its deadline
           \*     discovers the loss sooner: its next GET returns a
           \*     lease that does not name it (arming disjunct 3) or its
           \*     next PUT 409s against the recreated object's fresh rv
           \*     (disjunct 2). delVictims is cleaned on the victim's
           \*     next Steal, so this disjunct cannot mask the discovery
           \*     obligation of a LATER steal-deposition of the same
           \*     node.
           \/ stale \in delVictims

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
\* mechanism's failure mode). LeaderElectionDeletion.cfg injects exactly
\* that fault (MaxDeletes=1) and the invariant still holds; reverting (b)
\* alone -- genHW advancing at a dispatch-time Persist instead of inside
\* Steal -- falsifies it under deletion (see that cfg's header for the
\* trace). The claim is the load-bearing half once the operator can reset
\* the transition count.
\*
\* The invariant is ENABLED in the .cfg and holds over the full state
\* space. The deliberately-weakened test (run once during development,
\* NOT in CI): reverting Steal's derivation to the pre-fix
\* max(gen[n]+1, genHW+1) AND moving the genHW update back out of Steal
\* into a separate dispatch-time Persist action reproduces the depth-12
\* counterexample. That proves the encoding change is what makes the
\* invariant pass, not an accident of the surrounding edits.
\* -----------------------------------------------------------------------
\* Two cases on whether one of the believers actually holds the lease:
\*
\*   - A holder exists among the believers (every dual reachable without
\*     a deletion): the holder is the authorized leader and its
\*     generation must be STRICTLY GREATEST -- the executor's fence
\*     converges to the max heartbeat generation it has seen, so the
\*     stale believer's assignments are rejected and the holder's are
\*     accepted.
\*
\*   - No holder (reachable only by deleting the Lease out from under a
\*     dual -- both believers are now deposed): neither is authorized,
\*     so there is no "right" believer for the fence to prefer. The
\*     property degrades to DISTINCTNESS: the fence can still order
\*     them, the lower one is rejected, and the higher one keeps acting
\*     until the next real acquisition seeds from genHW+1 and exceeds
\*     both. Distinctness is not a weakening that can hide a collision:
\*     gen[m] = gen[n] fails both branches. And the holderless branch
\*     cannot hide a wrong ORDERING either -- the deletion does not
\*     change any generation, so the holderless state's generations are
\*     exactly the predecessor state's, which the holder-exists branch
\*     already checked with the strict inequality.
StaleLeaderHasStaleGeneration ==
  \A m, n \in Nodes :
    (state[m] = "Leading" /\ state[n] = "Leading" /\ m /= n) =>
      IF lease.holder \in {m, n}
      THEN LET stale == IF lease.holder = m THEN n ELSE m
           IN gen[stale] < gen[lease.holder]
      ELSE gen[m] /= gen[n]

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
  /\ deletes = 0
  /\ delVictims = {}

\* -----------------------------------------------------------------------
\* Time. clocks[n] advances independently. Ticks regardless of process
\* state (alive or not) -- CLOCK_MONOTONIC does not stop for a dead pod.
\* Bounded skew is a state CONSTRAINT, not a Tick precondition: it bounds
\* how far one node's clock can lag another's, not when Tick fires.
\*
\* The second conjunct encodes the production loop's tick discipline (the
\* LoopInterval assumption): run_lease_loop() calls maybe_self_fence()
\* every RENEW_INTERVAL, so a Leading node's clock cannot advance more
\* than FenceAfter + Renew past its last successful round-trip -- the loop would
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
       (clocks[n] + 1) - fence[n] <= FenceAfter + Renew
  /\ clocks' = [clocks EXCEPT ![n] = @ + 1]
  /\ UNCHANGED <<lease, alive, state, snap, obs, fence, gen, genHW, acquiredAt,
                 casRace, deletes, delVictims>>

\* -----------------------------------------------------------------------
\* Apiserver round-trip. Get(n) is the GET; Steal/RenewLease are the PUT
\* (each through ReplaceGuard); Conflict is the 409. The WRITE-COMPLETING
\* actions (Steal, RenewLease, Conflict) reset fence[n]; a bare Get does
\* NOT -- see the comment on Get(n).
\* -----------------------------------------------------------------------

\* GET the lease. obs[n] update follows decide_pure()'s ObservedUpdate:
\*   - holder = NULL    -> Clear (steal now, observed-record meaningless)
\*   - holder = n       -> Keep (we hold it; observed tracks OTHER holders)
\*   - holder = other, same rv we observed -> Keep (clock still running)
\*   - holder = other, new rv (or first observation) -> StartObserving
\*
\* fence[n] is UNCHANGED: a bare read does not reset the self-fence clock.
\* Production resets `last_successful_renew` at exactly two sites -- the
\* loop init (lib.rs:501, before the node has ever led) and the
\* `Ok(Ok(result))` arm of the COMPLETE try_acquire_or_renew() round-trip
\* (lib.rs:525). Every path through that function which returns Leading
\* ends in an rv-bumping write (create()'s POST or replace()'s PUT); the
\* paths that complete without a write return Standby or Conflict, both
\* of which set now_leading=false -- the node is not a believer
\* afterwards, so its fence value is moot. The combination "Leading, fresh
\* fence clock, rv unchanged" is therefore unreachable in production. A
\* model Get that refreshed fence[n] while leaving the node Leading and
\* the rv unchanged manufactured exactly that state, and it is what
\* falsified NeverDual in the asymmetric cfg: the victim's fence anchor
\* crept forward on every read while the thief's obs.since anchor stayed
\* at the last rv change, so the victim's Tick cap (anchored to its
\* fence) could outrun the thief's steal deadline (anchored to the rv) by
\* an unbounded number of reads. Anchoring both clocks to the same event
\* -- the last completed write -- is what makes the fence/steal
\* separation argument sound.
Get(n) ==
  /\ alive[n]
  /\ snap'  = [snap EXCEPT ![n] = lease]
  /\ obs'   = [obs EXCEPT ![n] =
       IF lease.holder = NULL THEN NULL
       ELSE IF lease.holder = n THEN obs[n]
       ELSE IF obs[n] /= NULL /\ obs[n].rv = lease.rv THEN obs[n]
       ELSE [rv |-> lease.rv, since |-> clocks[n]]]
  /\ UNCHANGED <<clocks, lease, alive, state, fence, gen, genHW, acquiredAt,
                 casRace, deletes, delVictims>>

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
        /\ clocks[n] - obs[n].since > StealAfter
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
  \* A fresh acquisition starts a fresh leadership term: n is no longer
  \* the victim of a previous deletion. Without this cleanup a stale
  \* delVictims entry would mask a later steal-deposition's discovery
  \* obligation (the fourth BoundedDualLeadership disjunct would fire
  \* for a victim that was deposed by a thief, not by a deletion).
  /\ delVictims' = delVictims \ {n}
  /\ UNCHANGED <<clocks, alive, deletes>>

\* The decide()->Renew path: we hold the lease, refresh renew_time. Bumps rv
\* (the apiserver bumps on every write); does NOT touch holder or the
\* transition count. Named RenewLease (not Renew) -- the constant Renew is
\* RENEW_INTERVAL.
\*
\* state' = Leading: a successful renew of a lease we hold means we lead,
\* whether we already believed it (no-op) or had stopped believing it (a
\* SelfFence false alarm, or a Crash/Recover cycle while the lease still
\* named us). The production correspondence is run_lease_loop()'s acquire
\* arm firing on the was_leading=false -> now_leading=true edge of a
\* successful renew: is_leader flips back to true and on_acquire(trans)
\* runs with the UNCHANGED transition count. Before this conjunct the
\* model under-approximated -- a self-fenced or recovered replica stayed
\* Following forever and TLC never explored the states where it leads
\* again.
\*
\* gen'/genHW' are the SAME on_acquire + recovery-claim collapse that
\* Steal performs (both run before recovery_complete ungates dispatch on
\* every acquire edge, renew-based or steal-based), with the renew's
\* UN-bumped transition count:
\*
\*   entry  = fetch_max(transitions + 1)      = max(gen[n], lease.gen + 1)
\*            (Steal's target is lease.gen + 2 because the steal PUT bumps
\*            the count first; a renew does not)
\*   seeded = the claim path's floor comparison: a floor ABOVE what the
\*            lease + the Arc gave us must be exceeded (someone -- possibly
\*            our own pre-crash incarnation -- claimed higher); a floor AT
\*            it is our own row and is retained.
\*
\* The three cases, each pinned by a TLC run:
\*   - SelfFence false alarm (never crashed): the Arc still holds our
\*     claimed generation, which equals genHW while we hold the lease --
\*     entry = gen[n], seeded = entry, genHW unchanged. The generation is
\*     RETAINED and no new claim row is written: the idempotent self
\*     re-claim (sched.lease.generation-claim's "retain own epoch"
\*     clause). A connectivity blip costs nothing.
\*   - Crash/Recover, no deletion: the Arc reset to 1; the transition
\*     count still encodes our generation (lease.gen + 1 = the value we
\*     originally derived), and our own claim row sits at exactly that
\*     floor -- entry = lease.gen + 1 = genHW, seeded = entry. RESTORED,
\*     no burn. Leaving gen UNCHANGED here instead falsifies
\*     StaleLeaderHasStaleGeneration in the BASE cfg at depth 15 (a
\*     holder that crashes inside a dual-belief window, recovers, and
\*     renews would lead at gen=1, below the believer it deposed) -- the
\*     red test for the entry half of this conjunct.
\*   - Crash/Recover after a DeleteLease: the recreated lease's
\*     transition count restarted from 0, so it no longer encodes the
\*     generation we acquired at (we derived it from the PG floor, not
\*     the count) -- entry under-restores to lease.gen + 1 < genHW, and
\*     the claim path bumps to genHW + 1 and writes a new claim row
\*     (production: pg_floor > gen_at_entry -> target = pg_floor + 1).
\*     Encoding only the entry half falsifies StaleLeaderHasStaleGeneration
\*     in the DELETION cfg at depth 9 (the under-restored holder collides
\*     with the deletion victim it cannot see) -- the red test for the
\*     seeded half. Both red-test traces are recorded in the cfgs'
\*     non-vacuity sections.
\* The seeded <= MaxGen precondition is the same state-space bound as
\* Steal's, for the same reason (a saturating clamp would manufacture
\* equal generations at the ceiling).
\*
\* acquiredAt is UNCHANGED: it records the rv of the CAS-guarded write
\* that made this node the HOLDER (the Steal), which is what casRace /
\* AtMostOneCASWinner are about -- two acquisitions racing at the same
\* resourceVersion. A renew is a CAS-guarded write but not an
\* acquisition; refreshing acquiredAt to the renew's rv would erase the
\* acquisition history the race detector reads. The rejected alternative
\* (refresh to snap[n].rv) is also SAFE -- lease.rv is monotonic, so no
\* later PUT can succeed at a generation's old acquisition rv either way
\* -- but it answers the wrong question. A Crash resets acquiredAt to
\* NULL; a recovered holder that re-acquires via renew therefore has
\* acquiredAt = NULL, which compares unequal to every rv and cannot flip
\* casRace.
RenewLease(n) ==
  LET entry  == IF gen[n] > lease.gen + 1 THEN gen[n] ELSE lease.gen + 1
      seeded == IF genHW > entry THEN genHW + 1 ELSE entry IN
  /\ alive[n]
  /\ snap[n] /= NULL /\ snap[n].holder = n
  /\ ReplaceGuard(n)
  /\ seeded <= MaxGen
  /\ lease' = [lease EXCEPT !.rv = snap[n].rv + 1]
  /\ state' = [state EXCEPT ![n] = "Leading"]
  /\ gen'   = [gen EXCEPT ![n] = seeded]
  /\ genHW' = IF seeded > genHW THEN seeded ELSE genHW
  /\ fence' = [fence EXCEPT ![n] = clocks[n]]
  /\ snap'  = [snap  EXCEPT ![n] = NULL]
  /\ UNCHANGED <<clocks, alive, obs, acquiredAt, deletes, delVictims>>

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
  /\ UNCHANGED <<clocks, lease, alive, obs, gen, genHW, acquiredAt, casRace,
                 deletes, delVictims>>

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
\* apiserver round-trip in > FenceAfter per its OWN clock flips is_leader=false
\* locally, without an apiserver write. The production loop checks this
\* every RENEW_INTERVAL; the model lets it fire any time the deadline has
\* passed (a superset of the production schedule -- sound for safety).
SelfFence(n) ==
  /\ alive[n]
  /\ state[n] = "Leading"
  /\ clocks[n] - fence[n] > FenceAfter
  /\ state' = [state EXCEPT ![n] = "Following"]
  /\ UNCHANGED <<clocks, lease, alive, snap, obs, fence, gen, genHW, acquiredAt,
                 casRace, deletes, delVictims>>

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
  /\ UNCHANGED <<clocks, lease, genHW, casRace, deletes, delVictims>>

\* Pod restart. The recovered process has no observation (its first
\* decide() returns StartObserving and waits a full StealAfter before stealing)
\* unless the lease still carries its identity (decide() returns Renew --
\* the recovered leader re-acquires its own lease without contention).
Recover(n) ==
  /\ ~alive[n]
  /\ alive' = [alive EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<clocks, lease, state, snap, obs, fence, gen, genHW, acquiredAt,
                 casRace, deletes, delVictims>>

\* Operator fault: `kubectl delete lease` destroys the Lease object and
\* the next replica to GET a 404 recreates it (election.rs::create()).
\* The two steps collapse into one action: the holder and the transition
\* count reset, the resourceVersion does NOT -- a recreated apiserver
\* object takes a fresh rv from the global etcd revision, so a snapshot
\* of the old incarnation can never satisfy ReplaceGuard's CAS against
\* the new one (the deposed-by-deletion holder's next PUT 409s, exactly
\* as production behaves). Resetting rv here would let a stale snap
\* spuriously pass the CAS and report an AtMostOneCASWinner violation
\* that no real apiserver admits.
\*
\* Everything else is UNCHANGED: the replicas do not observe the
\* deletion until their next GET; a Leading believer keeps believing
\* until its next round-trip exposes the loss or it self-fences; PG
\* (genHW) survives. That asymmetry -- the Lease's epoch source resets
\* while PG's does not -- is the fault this action injects, and the
\* write-ahead claim (genHW advancing inside Steal) is what survives it:
\* the next Steal of the recreated lease seeds from genHW+1, which
\* exceeds every generation ever acquired, even one whose holder never
\* dispatched anything. Without the claim (genHW advancing only at a
\* dispatch-time Persist), a post-deletion holder deposed before
\* persisting leaves genHW stale and its successor collides -- the
\* red-first counterexample documented in LeaderElectionDeletion.cfg.
\*
\* The current holder is recorded in delVictims: it was deposed by an
\* environmental fault rather than by a thief that observed it as stale,
\* so BoundedDualLeadership's steal-threshold coupling (disjunct 1) does
\* not apply to it -- see the fourth disjunct there.
\*
\* Guards: bounded by MaxDeletes (one deletion per trace is enough to
\* exhibit both the post-deletion dual and the post-deletion generation
\* collision); only a held lease (deleting an unheld lease changes
\* nothing any invariant constrains and only inflates the state space);
\* rv headroom for the recreation's fresh resourceVersion.
DeleteLease ==
  /\ deletes < MaxDeletes
  /\ lease.holder /= NULL
  /\ lease.rv < MaxRv
  /\ lease' = [holder |-> NULL, rv |-> lease.rv + 1, gen |-> 0]
  /\ deletes' = deletes + 1
  /\ delVictims' = delVictims \cup {lease.holder}
  /\ UNCHANGED <<clocks, alive, state, snap, obs, fence, gen, genHW,
                 acquiredAt, casRace>>

Next ==
  \/ \E n \in Nodes :
       \/ Tick(n)
       \/ Get(n)
       \/ Steal(n)
       \/ RenewLease(n)
       \/ Conflict(n)
       \/ SelfFence(n)
       \/ Crash(n)
       \/ Recover(n)
  \/ DeleteLease

Spec == Init /\ [][Next]_vars

==========================================================================
