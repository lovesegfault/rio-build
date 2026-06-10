// -*- mode: Bluespec; -*-
//
// The materialization listing-distribution law (bughunt-5 slot 6b,
// live_041; SIGNED Q4 full rendezvous; RULED CF-1/CF-2/CF-3):
// rendezvous-partitioned leader listings across the live store-worker
// membership, with the server-computed steal horizon for stale-owner
// jobs. Spec rule: sched.materialize.listing-distribution
// (docs/spec/components/scheduler.typ). The Rust lives in
// rio-scheduler/src/actor/materialize.rs (rendezvous_owner,
// ListingContacts, handle_list_materialization_jobs).
//
// WHAT IS MODELED
//   - members: per-WORKER composites (CF-2) — 2 replicas x 2 workers;
//     replica scale = BATCH join/leave of a replica's workers; crash =
//     a single worker dying. The scheduler's membership view is the
//     CONTACT map (who listed recently), never process liveness — a
//     dead-but-recently-listed worker still owns its slice, which is
//     exactly why the steal horizon exists.
//   - beat-list (CF-3): the server computes owner-slice UNION
//     steal-horizon per identity-bearing caller; the steal decision is
//     INSIDE the listing, no client steal lane. The caller's contact
//     is recorded BEFORE the serve computation (its own membership
//     rides the very call), mirroring the Rust order.
//   - claims: one-winner (a claimed job leaves the claimable set);
//     the serve-set guard on claimJob is the DISCOVERY abstraction —
//     a worker attempts only what a listing would carry right now.
//   - time: a beat clock `now` (tick bounded by MAX_TIME — the tlc
//     finiteness ceiling). STEAL_HORIZON and MEMBER_TTL are beat
//     counts; the Rust calibration (5 s / 60 s over a ~1 s poll beat)
//     maps to horizon 1 / ttl 3 at model scale: what matters is
//     horizon < ttl, so the silent-owner gap (stale but still a
//     member) is a reachable regime.
//
// DEFECT LANES (const-parameterized — the (cc)/(ccccc) discipline:
// every instance module binds ALL consts; the falsify twins import
// this module with a defect const flipped, no action overrides):
//   - SHARED_HEAD_CONVOY: the as-built pre-fix world — no owner law;
//     every member "owns" (and is served) the same head. Falsifies
//     ownerMapPartitionsClaimable + ownSliceListingsPartitionClaimable
//     (coverage holds, disjointness dies — the live convoy: 95.6% of
//     claim attempts wasted, 29.1 s fleet-wide stall).
//   - STEAL_ENABLED = false: partition without the steal horizon — a
//     dead owner's slice strands until the membership TTL re-keys.
//     Falsifies noJobUnlistedForever + stealRecoversOrphanedRemainder.
//   - EAGER_STEAL: the no-horizon greedy steal (the client-side /
//     unconditional-remainder design CF-3 rejected) — every caller is
//     served the whole remainder regardless of owner freshness.
//     Falsifies stealOnlyAfterOwnerStaleness (the prematureSteal
//     ghost latches at beat-list).
//
// EXACTLY-ONE-LISTING IS A NON-PROPERTY (RULED FS-3): the retired
// everyClaimableListedToExactlyOneLiveReplicaPerBeat was falsified by
// the steal action itself — the steal horizon legitimately
// double-lists a stale owner's slice until the owner returns.
// Duplication is BOUNDED BY OWNER STALENESS (stealOnlyAfterOwner-
// Staleness), never asserted away.
//
// GHOST DISCIPLINE: prematureSteal latches at beatList with a
// semantically-inert non-identity write in the live regime (the
// stolen set is stale-owned by construction, so the fresh-owner
// existential is structurally false — the quint-policy P4 allowance;
// the eager-steal twin is the teeth, P1).
//
// State counts, depth, wall-clock: in the wired checks' transcripts
// and the introducing commit message, never here.
module materializationDistribution {
  /// Beat-clock ceiling (tlc finiteness; tick precondition).
  const MAX_TIME: int
  /// Steal horizon in beats: an owner whose last listing is OLDER
  /// than this has missed its beat; its slice serves broadly.
  const STEAL_HORIZON: int
  /// Membership TTL in beats (> STEAL_HORIZON): silence past this
  /// drops the member from the contact map — the partition re-keys.
  const MEMBER_TTL: int
  /// Defect lane: the as-built shared-head world (no owner law).
  const SHARED_HEAD_CONVOY: bool
  /// Defect lane: partition without the steal horizon.
  const STEAL_ENABLED: bool
  /// Defect lane: the no-horizon greedy steal.
  const EAGER_STEAL: bool

  /// Per-worker composite members (CF-2): {replica}w{worker}.
  val MEMBERS = Set("r0w0", "r0w1", "r1w0", "r1w1")
  val REPLICAS = Set("r0", "r1")
  val JOBS = Set("j0", "j1", "j2")

  /// Replica batch membership (CF-2: scale events join/leave whole
  /// worker batches).
  pure def workersOf(r: str): Set[str] =
    if (r == "r0") Set("r0w0", "r0w1") else Set("r1w0", "r1w1")

  // --- state ----------------------------------------------------------------

  /// The beat clock.
  var now: int
  /// Process liveness (can act). The scheduler NEVER reads this —
  /// its view is lastListed only.
  var live: str -> bool
  /// The leader's contact map: last identity-bearing listing beat;
  /// -1 = never listed (not a member).
  var lastListed: str -> int
  /// One-winner claims (a claimed job leaves the claimable set).
  var claimed: Set[str]
  /// Ghost: a beat-list served a non-own job whose owner was FRESH
  /// (the premature steal — impossible in the live regime, the
  /// EAGER_STEAL twin's witness).
  var prematureSteal: bool

  // --- pure law ---------------------------------------------------------------

  pure def jIdx(j: str): int = if (j == "j0") 0 else if (j == "j1") 1 else 2
  pure def mIdx(m: str): int =
    if (m == "r0w0") 0 else if (m == "r0w1") 1 else if (m == "r1w0") 2 else 3

  /// The HRW score (abstracted: any fixed per-(job, member) table
  /// injective per job across members — 13 is a unit mod 31, so the
  /// four member scores are distinct for every job offset).
  pure def prio(j: str, m: str): int = (jIdx(j) * 7 + mIdx(m) * 13) % 31

  /// The scheduler's membership view AT a contact map: members whose
  /// last listing is within MEMBER_TTL of t.
  pure def membersAt(ll: str -> int, t: int, ttl: int): Set[str] =
    MEMBERS.filter(m => ll.get(m) >= 0 and t - ll.get(m) <= ttl)

  /// Owner freshness AT a contact map: listed within the steal
  /// horizon.
  pure def freshAt(ll: str -> int, t: int, horizon: int, m: str): bool =
    (ll.get(m) >= 0 and t - ll.get(m) <= horizon)

  /// THE owner map (the single partition source — mirrors
  /// rendezvous_owner). Singleton by construction in the live law
  /// (injective scores); the SHARED_HEAD_CONVOY lane is the as-built
  /// degenerate map: everyone owns everything.
  pure def ownersOf(convoy: bool, j: str, ms: Set[str]): Set[str] =
    if (convoy) ms
    else ms.filter(m => ms.forall(o => (o == m) or (prio(j, m) > prio(j, o))))

  /// One caller's serve set (beat-list AND the claim-discovery
  /// abstraction): own slice UNION the steal horizon — owner-slice
  /// and steal derived from the SAME ownersOf source at the same
  /// site (CF-3). Solo membership and the convoy lane serve the
  /// whole head (the unpartitioned fallbacks).
  pure def serveSetFor(
    convoy: bool, stealOn: bool, eager: bool, horizon: int,
    me: str, ms: Set[str], ll: str -> int, t: int, cl: Set[str]
  ): Set[str] =
    if (ms.size() <= 1) cl
    else cl.filter(j =>
      val owners = ownersOf(convoy, j, ms)
      if (owners.contains(me)) true
      else if (eager) true
      else if (stealOn) owners.forall(o => not(freshAt(ll, t, horizon, o)))
      else false)

  // --- actions ----------------------------------------------------------------

  /// The beat clock advances (bounded — tlc finiteness).
  action tick: bool = all {
    now < MAX_TIME,
    now' = now + 1,
    live' = live, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal,
  }

  /// An identity-bearing listing call: the caller's contact is
  /// recorded FIRST (its membership rides this very call — the Rust
  /// order), then the server computes owner-slice UNION steal-horizon
  /// over the updated map. The ghost latches if any served NON-own
  /// job has a FRESH owner (structurally false in the live regime —
  /// the P4-allowance inert write; EAGER_STEAL makes it reachable).
  action beatList(m: str): bool = all {
    live.get(m),
    val ll1 = lastListed.set(m, now)
    val ms = membersAt(ll1, now, MEMBER_TTL)
    val served = serveSetFor(
      SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, STEAL_HORIZON,
      m, ms, ll1, now, JOBS.exclude(claimed))
    val stolen = served.filter(j => not(ownersOf(SHARED_HEAD_CONVOY, j, ms).contains(m)))
    all {
      lastListed' = ll1,
      (prematureSteal' = (prematureSteal or
        stolen.exists(j =>
          ownersOf(SHARED_HEAD_CONVOY, j, ms).exists(o => freshAt(ll1, now, STEAL_HORIZON, o))))),
      now' = now, live' = live, claimed' = claimed,
    }
  }

  /// A worker claims a job its CURRENT listing would carry (the
  /// discovery abstraction); one-winner arbitration = the claimed
  /// set.
  action claimJob(m: str, j: str): bool = all {
    live.get(m),
    not(claimed.contains(j)),
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    serveSetFor(
      SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, STEAL_HORIZON,
      m, ms, lastListed, now, JOBS.exclude(claimed)).contains(j),
    claimed' = claimed.union(Set(j)),
    now' = now, live' = live, lastListed' = lastListed,
    prematureSteal' = prematureSteal,
  }

  /// Replica scale-out: a BATCH join of the replica's workers
  /// (CF-2). Contacts are NOT reset — the rejoin-with-same-name case
  /// is the harsher interleaving and production pods rejoin fresh.
  action joinReplica(r: str): bool = all {
    workersOf(r).exists(w => not(live.get(w))),
    live' = MEMBERS.mapBy(m => (live.get(m) or workersOf(r).contains(m))),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal,
  }

  /// Replica scale-in: a BATCH leave (CF-2).
  action leaveReplica(r: str): bool = all {
    workersOf(r).exists(w => live.get(w)),
    live' = MEMBERS.mapBy(m => (live.get(m) and not(workersOf(r).contains(m)))),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal,
  }

  /// A single worker dies (the per-worker churn axis: its contact
  /// entry survives until the TTL — the scheduler cannot see the
  /// death, only the silence).
  action crashWorker(m: str): bool = all {
    live.get(m),
    live' = live.set(m, false),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal,
  }

  // --- harness ------------------------------------------------------------------

  action init = all {
    now' = 0,
    live' = MEMBERS.mapBy(m => true),
    lastListed' = MEMBERS.mapBy(m => -1),
    claimed' = Set(),
    prematureSteal' = false,
  }

  action step = {
    nondet m = MEMBERS.oneOf()
    nondet r = REPLICAS.oneOf()
    nondet j = JOBS.oneOf()
    any {
      tick,
      beatList(m),
      claimJob(m, j),
      joinReplica(r),
      leaveReplica(r),
      crashWorker(m),
    }
  }

  // --- invariants -----------------------------------------------------------------

  /// Every claimable job has EXACTLY ONE member of the scheduler's
  /// membership view as its rendezvous owner — disjointness +
  /// coverage of OWNERSHIP (the convoy twin's first kill:
  /// the degenerate map owns everything everywhere).
  val ownerMapPartitionsClaimable =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    (ms.size() > 0) implies
      JOBS.exclude(claimed).forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).size() == 1)

  /// The OWN-SLICE segments of the fresh members' listings are
  /// pairwise disjoint, and a claimable job with a FRESH owner is in
  /// that owner's own segment (coverage of the served head). Under
  /// the convoy lane coverage holds and disjointness dies — the
  /// recorded live shape.
  val ownSliceListingsPartitionClaimable =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val ownSeg = ms.mapBy(m =>
      JOBS.exclude(claimed).filter(j => ownersOf(SHARED_HEAD_CONVOY, j, ms).contains(m)))
    and {
      ms.forall(m1 => ms.forall(m2 =>
        (m1 == m2) or ownSeg.get(m1).intersect(ownSeg.get(m2)).size() == 0)),
      JOBS.exclude(claimed).forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
          freshAt(lastListed, now, STEAL_HORIZON, o) implies ownSeg.get(o).contains(j))),
    }

  /// Bounded duplication: a job is served to a NON-owner only when
  /// its owner has missed its beat (the prematureSteal ghost stays
  /// false — the EAGER_STEAL twin's witness).
  val stealOnlyAfterOwnerStaleness = not(prematureSteal)

  /// Liveness-as-safety over the bounded silence horizon: while any
  /// live scheduler-visible member exists, every claimable job is
  /// either servable RIGHT NOW to some live member, or its owner is
  /// still within the steal-horizon grace (the accepted orphan
  /// latency). The no-steal twin violates in the (horizon, ttl]
  /// gap: the dead owner's slice is nobody's until the TTL re-keys.
  val noJobUnlistedForever =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    ms.exists(m => live.get(m)) implies
      JOBS.exclude(claimed).forall(j =>
        ms.exists(m => and {
          live.get(m),
          serveSetFor(
            SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, STEAL_HORIZON,
            m, ms, lastListed, now, JOBS.exclude(claimed)).contains(j),
        })
        or
        ownersOf(SHARED_HEAD_CONVOY, j, ms).exists(o =>
          freshAt(lastListed, now, STEAL_HORIZON, o)))

  /// The steal horizon DELIVERS the orphaned remainder: a claimable
  /// job whose owners are all past the steal horizon is in EVERY
  /// member's serve set (served broadly, not just to someone).
  val stealRecoversOrphanedRemainder =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    (ms.size() > 1) implies
      JOBS.exclude(claimed).forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
          not(freshAt(lastListed, now, STEAL_HORIZON, o)))
        implies
        ms.forall(m =>
          serveSetFor(
            SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, STEAL_HORIZON,
            m, ms, lastListed, now, JOBS.exclude(claimed)).contains(j)))
}

/// The wired live regime: horizon 1 < ttl 3 < MAX_TIME 6 keeps the
/// silent-owner gap (stale-but-member) and the TTL re-key both
/// reachable; defect consts all off.
module materializationDistributionBase {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false
  ).*
}
