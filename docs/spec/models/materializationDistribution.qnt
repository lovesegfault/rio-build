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
//     ownerMapPartitionsClaimable + serveSliceDisjointWhenFresh
//     (coverage holds, disjointness dies — the live convoy: 95.6% of
//     claim attempts wasted, 29.1 s fleet-wide stall).
//   - STEAL_ENABLED = false: partition without the steal horizon — a
//     dead owner's slice strands until the membership TTL re-keys.
//     Falsifies noJobUnlistedForever + stealRecoversOrphanedRemainder.
//   - EAGER_STEAL: the no-horizon greedy steal (the client-side /
//     unconditional-remainder design CF-3 rejected) — every caller is
//     served the whole remainder regardless of owner freshness.
//     Falsifies stealOnlyAfterOwnerStaleness (the prematureSteal
//     ghost latches at beat-list) AND serveSliceDisjointWhenFresh —
//     the ISOLATED disjointness kill (ownersOf is untouched by the
//     eager lane, so ownership stays exactly-one while the serve
//     plane overlaps).
//   - OWNER_SERVE_SKEW (merged from bug_087's re-aim): the SERVE
//     side consults a skewed owner view (inverted score) — the model
//     analog of the CF-2 listing-arm/owner-map divergence
//     (rendezvous-hash vs sanitize-salt disagreement). Falsifies
//     serveSliceCoversFreshOwned (the map owner's beat does not
//     serve its fresh-owned job) while ownerMapPartitionsClaimable
//     AND serveSliceDisjointWhenFresh hold (the skewed view is
//     equally injective) — the world where owning and serving
//     disagree, the exact production failure the coverage law
//     exists to forbid.
//   - DEAD_HEAD_SERVED (round-12 WO-S9-8, live_061 — the dead-head
//     face, the JOB-side dual of the wedge axis): a DEAD job (its
//     node completed by other means while the job sat unresolved —
//     the live_061 zombie: every claim against it answers Gone, so it
//     can never convert) is STILL SERVED — the as-built pre-fix world
//     where the listing carried node-terminal jobs (db query
//     node-blind, serve filter claimability-only). The dead head then
//     pins every serve set it lands in: claim attempts burn on it
//     forever and the oldest-first window never drains (live_061's
//     ~0.5% fleet conversion). Falsifies
//     servedJobsConvertibleWithinHorizon. The live law (the rust
//     node-face exclusion, sched.materialize.claimability-projection)
//     removes a dead job from the convertible plane at the listing
//     itself; its terminal disposition (the obsolete fold) is the JOB
//     model's plane (materializationJob.qnt obsoleteOnProduced), not
//     this model's — the dead axis here pins only the LISTING-plane
//     exclusion law. The death axis (deadAt + dieJob) is gated on
//     DEAD_AXIS_ENABLED and explored in the THIRD main
//     (materializationDistributionDead) over a single designated
//     deadable job (DEADABLE = j0) — the bounded exhaustive census,
//     the WEDGEABLE precedent verbatim; the base and wedge mains bind
//     the axis OFF and keep their censuses (deadAt is frozen there).
//   - WEDGED_KEEPS_BEATING (round-6 WO-S1-4, merged_bug_005 — the
//     honest-beat law, store.materialize.honest-beat): a WEDGED
//     worker (alive, listing, but conversion-incapable — budget
//     pinned, ledger at cap, or every mint refused) KEEPS beating.
//     Its slice then never enters anyone's steal horizon: the
//     wedged-but-polling worker pins ~1/N of the claimable head
//     fleet-wide. Falsifies wedgedOwnerSliceRecovered. The live law
//     (the rust honest-beat gate) withholds the beat instead, so
//     capability loss ages into the steal horizon exactly like
//     silence does. The capability axis (wedgedAt + the wedge and
//     unwedge actions) is gated on WEDGE_AXIS_ENABLED and explored
//     in the SECOND main (materializationDistributionWedge) over a
//     single designated wedgeable member (WEDGEABLE = r1w0, an
//     owner at full membership) — the bounded exhaustive census;
//     the base main binds the axis OFF and keeps its original
//     census byte-for-byte (wedgedAt is frozen there).
//
// BEAT GRANULARITY NOTE (bug_045 / sched.materialize.listing-cost):
// the Rust listing arm now serves polls from a beat-scoped snapshot
// (TTL = 1 s) with per-beat staleness; the model's tick already IS
// the beat, so the cache sits below this abstraction — no model
// change for the cost law (its witnesses are Rust operation
// counters).
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
  /// Defect lane: the serve plane consults a SKEWED owner view
  /// (inverted score) — owning and serving disagree (the CF-2
  /// divergence class).
  const OWNER_SERVE_SKEW: bool
  /// Capability axis master switch (round-6 WO-S1-4): the wedge and
  /// unwedge actions exist only when enabled. The base main binds
  /// false — its census is byte-identical to the pre-axis model
  /// (wedgedAt is frozen at all-capable).
  const WEDGE_AXIS_ENABLED: bool
  /// Defect lane (the honest-beat violation, merged_bug_005): a
  /// wedged worker KEEPS issuing listing beats — the as-built
  /// pre-fix world where listing recency lies about claim
  /// capability.
  const WEDGED_KEEPS_BEATING: bool
  /// Death axis master switch (round-12 WO-S9-8): the dieJob action
  /// exists only when enabled. The base and wedge mains bind false —
  /// their censuses are unchanged (deadAt is frozen at
  /// all-convertible).
  const DEAD_AXIS_ENABLED: bool
  /// Defect lane (live_061): a dead job is STILL SERVED — no
  /// node-face exclusion at the listing (the as-built pre-fix world
  /// where the dead head pins the window).
  const DEAD_HEAD_SERVED: bool

  /// Per-worker composite members (CF-2): {replica}w{worker}.
  val MEMBERS = Set("r0w0", "r0w1", "r1w0", "r1w1")
  val REPLICAS = Set("r0", "r1")
  val JOBS = Set("j0", "j1", "j2")

  /// Replica batch membership (CF-2: scale events join/leave whole
  /// worker batches).
  pure def workersOf(r: str): Set[str] =
    if (r == "r0") Set("r0w0", "r0w1") else Set("r1w0", "r1w1")

  /// The designated wedgeable member (the bounded wedge-axis census:
  /// one member's wedge state keeps the second main exhaustively
  /// checkable). Premise reachability is MACHINE-CHECKED, never a
  /// verified-at-a-commit note (bug_083): the wired expect-violation
  /// witness (canReachWedgedPastHorizonOwner, the
  /// quint-matdist-wedge-premise-reachable entry) goes red the moment
  /// the wedged-owner premise becomes unreachable in the live wedge
  /// regime. The recovery law itself quantifies over ALL owners.
  val WEDGEABLE = Set("r1w0")

  /// The designated deadable job (the bounded death-axis census: one
  /// job's dead state keeps the third main exhaustively checkable —
  /// the WEDGEABLE precedent on the job axis). Premise reachability
  /// is MACHINE-CHECKED, never a verified-at-a-commit note (bug_083):
  /// the wired expect-violation witness (canReachDeadHeadPastHorizon,
  /// the quint-matdist-dead-premise-reachable entry) goes red the
  /// moment the dead-head premise becomes unreachable in the live
  /// dead regime. The exclusion law itself quantifies over ALL jobs.
  val DEADABLE = Set("j0")

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
  /// Capability axis (round-6 WO-S1-4): the beat a member became
  /// conversion-incapable (wedged), or -1 = capable. The honest-beat
  /// gate reads it; only [WEDGEABLE] members ever move it (the
  /// bounded census).
  var wedgedAt: str -> int
  /// Death axis (round-12 WO-S9-8): the beat a job's node completed
  /// by other means while the job sat unresolved (the live_061
  /// zombie face), or -1 = convertible. The node-face exclusion
  /// reads it; only [DEADABLE] jobs ever move it (the bounded
  /// census).
  var deadAt: str -> int

  // --- pure law ---------------------------------------------------------------

  /// Conversion capability: not currently wedged.
  pure def canConvertAt(w: str -> int, m: str): bool = w.get(m) < 0

  /// Job convertibility: the node has not completed by other means
  /// (a dead job's every claim answers Gone — it can never convert).
  pure def jobConvertibleAt(da: str -> int, j: str): bool = da.get(j) < 0

  /// THE serve-plane input (the node-face exclusion,
  /// sched.materialize.claimability-projection): the live law lists
  /// only convertible claimable jobs — a dead job leaves the plane at
  /// the listing itself (its terminal disposition is the job model's
  /// obsolete fold, below this abstraction). The DEAD_HEAD_SERVED
  /// lane is the as-built world: the listing carries the dead head.
  pure def servablePlane(deadServed: bool, da: str -> int, cl: Set[str]): Set[str] =
    if (deadServed) cl else cl.filter(j => jobConvertibleAt(da, j))

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

  /// The SERVE-side score: identical to `prio` in the live law; the
  /// OWNER_SERVE_SKEW lane inverts it (still injective per job — the
  /// scores stay distinct), so the serving arms and the owner map
  /// pick DIFFERENT members whenever the view has two or more.
  pure def servePrio(skew: bool, j: str, m: str): int =
    if (skew) 30 - prio(j, m) else prio(j, m)

  /// The owner view the SERVE PLANE consults (the bug_087 re-aim
  /// seam): in the live law this is exactly `ownersOf`; under
  /// OWNER_SERVE_SKEW the serve plane disagrees with the map — the
  /// representable defect the coverage leaf exists to kill.
  pure def serveOwnersOf(convoy: bool, skew: bool, j: str, ms: Set[str]): Set[str] =
    if (convoy) ms
    else ms.filter(m => ms.forall(o => (o == m) or (servePrio(skew, j, m) > servePrio(skew, j, o))))

  /// One caller's serve set (beat-list AND the claim-discovery
  /// abstraction): own slice UNION the steal horizon — owner-slice
  /// and steal derived from the SAME ownersOf source at the same
  /// site (CF-3). Solo membership and the convoy lane serve the
  /// whole head (the unpartitioned fallbacks). The OWN arm consults
  /// the SERVE-side owner view (`serveOwnersOf` — identical to the
  /// map in the live law; the OWNER_SERVE_SKEW lane diverges exactly
  /// here, the CF-2 listing-arm seam); the STALENESS gate stays on
  /// the real owner map (freshness of the member the partition law
  /// names — the steal contract is about the MAP owner missing its
  /// beat).
  pure def serveSetFor(
    convoy: bool, stealOn: bool, eager: bool, skew: bool, horizon: int,
    me: str, ms: Set[str], ll: str -> int, t: int, cl: Set[str]
  ): Set[str] =
    if (ms.size() <= 1) cl
    else cl.filter(j =>
      val owners = ownersOf(convoy, j, ms)
      val serveOwners = serveOwnersOf(convoy, skew, j, ms)
      if (serveOwners.contains(me)) true
      else if (eager) true
      else if (stealOn) owners.forall(o => not(freshAt(ll, t, horizon, o)))
      else false)

  // --- actions ----------------------------------------------------------------

  /// The beat clock advances (bounded — tlc finiteness).
  action tick: bool = all {
    now < MAX_TIME,
    now' = now + 1,
    live' = live, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal, wedgedAt' = wedgedAt,
    deadAt' = deadAt,
  }

  /// An identity-bearing listing call: the caller's contact is
  /// recorded FIRST (its membership rides this very call — the Rust
  /// order), then the server computes owner-slice UNION steal-horizon
  /// over the updated map. The ghost latches if any served NON-own
  /// job has a FRESH owner (structurally false in the live regime —
  /// the P4-allowance inert write; EAGER_STEAL makes it reachable).
  action beatList(m: str): bool = all {
    live.get(m),
    // The honest beat (merged_bug_005, store.materialize.honest-beat):
    // a pass that cannot convert withholds the listing — UNLESS the
    // WEDGED_KEEPS_BEATING defect lane is on (the as-built pre-fix
    // world the falsify twin certifies).
    (WEDGED_KEEPS_BEATING or canConvertAt(wedgedAt, m)),
    val ll1 = lastListed.set(m, now)
    val ms = membersAt(ll1, now, MEMBER_TTL)
    val served = serveSetFor(
      SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
      m, ms, ll1, now, servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed)))
    val stolen = served.filter(j => not(ownersOf(SHARED_HEAD_CONVOY, j, ms).contains(m)))
    all {
      lastListed' = ll1,
      (prematureSteal' = (prematureSteal or
        stolen.exists(j =>
          ownersOf(SHARED_HEAD_CONVOY, j, ms).exists(o => freshAt(ll1, now, STEAL_HORIZON, o))))),
      now' = now, live' = live, claimed' = claimed,
      wedgedAt' = wedgedAt, deadAt' = deadAt,
    }
  }

  /// A worker claims a job its CURRENT listing would carry (the
  /// discovery abstraction); one-winner arbitration = the claimed
  /// set. A wedged worker can never convert (the capability the
  /// honest beat witnesses).
  action claimJob(m: str, j: str): bool = all {
    live.get(m),
    canConvertAt(wedgedAt, m),
    // A dead job NEVER converts (every claim answers Gone) — in the
    // defect lane it stays served and burns attempts; this guard is
    // what makes the unexcluded dead head a pinned head rather than
    // a drained one.
    jobConvertibleAt(deadAt, j),
    not(claimed.contains(j)),
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    serveSetFor(
      SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
      m, ms, lastListed, now,
      servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))).contains(j),
    claimed' = claimed.union(Set(j)),
    now' = now, live' = live, lastListed' = lastListed,
    prematureSteal' = prematureSteal, wedgedAt' = wedgedAt, deadAt' = deadAt,
  }

  /// Replica scale-out: a BATCH join of the replica's workers
  /// (CF-2). Contacts are NOT reset — the rejoin-with-same-name case
  /// is the harsher interleaving and production pods rejoin fresh.
  action joinReplica(r: str): bool = all {
    workersOf(r).exists(w => not(live.get(w))),
    live' = MEMBERS.mapBy(m => (live.get(m) or workersOf(r).contains(m))),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal, wedgedAt' = wedgedAt,
    deadAt' = deadAt,
  }

  /// Replica scale-in: a BATCH leave (CF-2).
  action leaveReplica(r: str): bool = all {
    workersOf(r).exists(w => live.get(w)),
    live' = MEMBERS.mapBy(m => (live.get(m) and not(workersOf(r).contains(m)))),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal, wedgedAt' = wedgedAt,
    deadAt' = deadAt,
  }

  /// A single worker dies (the per-worker churn axis: its contact
  /// entry survives until the TTL — the scheduler cannot see the
  /// death, only the silence).
  action crashWorker(m: str): bool = all {
    live.get(m),
    live' = live.set(m, false),
    now' = now, lastListed' = lastListed, claimed' = claimed,
    prematureSteal' = prematureSteal, wedgedAt' = wedgedAt,
    deadAt' = deadAt,
  }

  /// The capability axis (round-6 WO-S1-4): a member becomes
  /// conversion-incapable — budget pinned by an unanswered mint, the
  /// resume ledger at cap, or a conversion-futility streak. The
  /// member stays LIVE (the wedged-but-polling class); only
  /// [WEDGEABLE] members wedge (the bounded census).
  action wedgeWorker(m: str): bool = all {
    WEDGE_AXIS_ENABLED,
    WEDGEABLE.contains(m),
    live.get(m),
    canConvertAt(wedgedAt, m),
    wedgedAt' = wedgedAt.set(m, now),
    now' = now, live' = live, lastListed' = lastListed,
    claimed' = claimed, prematureSteal' = prematureSteal,
    deadAt' = deadAt,
  }

  /// Capability returns (the futility latch's probe converting, the
  /// orphan answering, the ledger draining).
  action unwedgeWorker(m: str): bool = all {
    WEDGE_AXIS_ENABLED,
    not(canConvertAt(wedgedAt, m)),
    wedgedAt' = wedgedAt.set(m, -1),
    now' = now, live' = live, lastListed' = lastListed,
    claimed' = claimed, prematureSteal' = prematureSteal,
    deadAt' = deadAt,
  }

  /// The death axis (round-12 WO-S9-8, live_061): a job's node
  /// completes by other means while the job sits unresolved — the
  /// store probe found the outputs, a sibling build produced them,
  /// CA cutoff. From this beat every claim against the job answers
  /// Gone. Only [DEADABLE] jobs die (the bounded census); the job
  /// model owns the death's terminal disposition (the obsolete
  /// fold) — here it is an absorbing fact the listing law must
  /// exclude.
  action dieJob(j: str): bool = all {
    DEAD_AXIS_ENABLED,
    DEADABLE.contains(j),
    not(claimed.contains(j)),
    jobConvertibleAt(deadAt, j),
    deadAt' = deadAt.set(j, now),
    now' = now, live' = live, lastListed' = lastListed,
    claimed' = claimed, prematureSteal' = prematureSteal,
    wedgedAt' = wedgedAt,
  }

  // --- harness ------------------------------------------------------------------

  action init = all {
    now' = 0,
    live' = MEMBERS.mapBy(m => true),
    lastListed' = MEMBERS.mapBy(m => -1),
    claimed' = Set(),
    prematureSteal' = false,
    wedgedAt' = MEMBERS.mapBy(m => -1),
    deadAt' = JOBS.mapBy(j => -1),
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
      wedgeWorker(m),
      unwedgeWorker(m),
      dieJob(j),
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
      servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed)).forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).size() == 1)

  /// LEAF 1 of the listing law, restated over the LAW FUNCTION
  /// (bug_087 re-aim — the retired ownSliceListingsPartitionClaimable
  /// re-derived ownersOf in its own definition, so its coverage
  /// conjunct was definitionally true and its disjointness conjunct
  /// was the sibling's negation; it never consulted serveSetFor):
  /// for distinct members of the scheduler's view, the serve sets
  /// restricted to FRESH-owned claimable jobs are pairwise disjoint —
  /// a fresh-owned job is served ONLY by its owner (the steal lane is
  /// staleness-gated, so this constrains serveSetFor's arm
  /// structure). Dies under convoy (alongside ownership — the
  /// as-built world) and under eager (the ISOLATED kill: ownersOf
  /// untouched, ownership intact in the same lane).
  val serveSliceDisjointWhenFresh =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val cl = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    val freshOwned = cl.filter(j =>
      ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
        freshAt(lastListed, now, STEAL_HORIZON, o)))
    ms.forall(m1 => ms.forall(m2 =>
      (m1 == m2) or
      serveSetFor(
        SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
        m1, ms, lastListed, now, cl)
        .intersect(serveSetFor(
          SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
          m2, ms, lastListed, now, cl))
        .intersect(freshOwned)
        .size() == 0))

  /// LEAF 2: coverage THROUGH the law function — every claimable job
  /// with a FRESH owner is in that owner's OWN serve output. Not a
  /// re-derivation of ownersOf: the proposition is that the thing
  /// the Rust listing arm mirrors (serveSetFor) actually DELIVERS
  /// the fresh-owned job to its owner. Falsifiable for the first
  /// time: dies under OWNER_SERVE_SKEW (the map owner's beat does
  /// not serve its fresh-owned job) while ownership AND disjointness
  /// hold in the same lane — the world where owning and serving
  /// disagree.
  val serveSliceCoversFreshOwned =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val cl = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    cl.forall(j =>
      ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
        freshAt(lastListed, now, STEAL_HORIZON, o) implies
          serveSetFor(
            SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
            o, ms, lastListed, now, cl).contains(j)))

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
    val plane = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    ms.exists(m => live.get(m)) implies
      plane.forall(j =>
        ms.exists(m => and {
          live.get(m),
          serveSetFor(
            SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
            m, ms, lastListed, now, plane).contains(j),
        })
        or
        ownersOf(SHARED_HEAD_CONVOY, j, ms).exists(o =>
          freshAt(lastListed, now, STEAL_HORIZON, o)))

  /// The honest-beat recovery law (round-6 WO-S1-4, merged_bug_005;
  /// verified via the nix/quint.nix wedge-recovery + wedged-poller
  /// entries — the markers live at the WIRING, not here):
  /// a claimable job whose rendezvous owner is WEDGED (alive but
  /// conversion-incapable) past the steal-horizon grace measured
  /// from the WEDGE ONSET is in some capable live member's serve set
  /// — the withheld beat ages the wedged owner into the steal
  /// horizon, so capability loss re-homes the slice exactly like
  /// silence does (the stealRecoversOrphanedRemainder shape re-keyed
  /// on capability). Under WEDGED_KEEPS_BEATING the wedged poller
  /// stays eternally fresh and its slice never re-homes — the
  /// falsify twin's kill. P7: a named single-proposition leaf (one
  /// implication chain; no anonymous conjunct bundle).
  val wedgedOwnerSliceRecovered =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val plane = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    ms.exists(m => live.get(m) and canConvertAt(wedgedAt, m)) implies
      plane.forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
          (live.get(o) and not(canConvertAt(wedgedAt, o)) and now - wedgedAt.get(o) > STEAL_HORIZON)
            implies
          ms.exists(m => and {
            live.get(m),
            canConvertAt(wedgedAt, m),
            serveSetFor(
              SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
              m, ms, lastListed, now, plane).contains(j),
          })))

  /// bug_083 (round-8 WO-S2-5) — the premise-reachability witness for
  /// wedgedOwnerSliceRecovered, wired as an EXPECT-VIOLATION check on
  /// the LIVE wedge regime (the quint-matdist-wedge-premise-reachable
  /// entry in nix/quint.nix — markers live at the wiring, per the
  /// header discipline): VIOLATED exactly when the law's contended
  /// premise — a live, wedged-past-horizon owner of a claimable job,
  /// with a capable live member in the view — is reached WITHOUT the
  /// defect lane's WEDGED_KEEPS_BEATING freshness (the falsify twin's
  /// kill certifies reachability only under that mechanism, which the
  /// live gate forbids — a strictly weaker proposition). In the live
  /// main the premise exists only through the corridor
  /// wedgedAt + STEAL_HORIZON < now <= lastListed + MEMBER_TTL, open
  /// solely because STEAL_HORIZON < MEMBER_TTL in the wedge
  /// instantiation: a retune to MEMBER_TTL <= STEAL_HORIZON, a
  /// wedgeWorker guard narrowing, or a membersAt/freshAt change
  /// closes it and vacuates the law with the three wedge checks still
  /// green — THIS check goes red instead.
  val canReachWedgedPastHorizonOwner =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    not(servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed)).exists(j =>
      ownersOf(SHARED_HEAD_CONVOY, j, ms).exists(o => and {
        live.get(o),
        not(canConvertAt(wedgedAt, o)),
        now - wedgedAt.get(o) > STEAL_HORIZON,
        ms.exists(m => live.get(m) and canConvertAt(wedgedAt, m)),
      })))

  /// The steal horizon DELIVERS the orphaned remainder: a claimable
  /// job whose owners are all past the steal horizon is in EVERY
  /// member's serve set (served broadly, not just to someone).
  val stealRecoversOrphanedRemainder =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val plane = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    (ms.size() > 1) implies
      plane.forall(j =>
        ownersOf(SHARED_HEAD_CONVOY, j, ms).forall(o =>
          not(freshAt(lastListed, now, STEAL_HORIZON, o)))
        implies
        ms.forall(m =>
          serveSetFor(
            SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
            m, ms, lastListed, now, plane).contains(j)))

  /// The dead-head exclusion law (round-12 WO-S9-8, live_061;
  /// verified via the nix/quint.nix dead-regime entries — the markers
  /// live at the WIRING, not here): every job in any member's serve
  /// set is CONVERTIBLE, or its death is younger than the
  /// steal-horizon grace (the bound production promises across the
  /// beat-cache and persist-lag windows; the live node-face exclusion
  /// removes a dead job at the very next listing read). Under
  /// DEAD_HEAD_SERVED the dead head stays served as `now` advances
  /// past the grace — the falsify twin's kill: the pinned window,
  /// live_061's claim-attempt burn. P7: a named single-proposition
  /// leaf (one implication chain; no anonymous conjunct bundle).
  val servedJobsConvertibleWithinHorizon =
    val ms = membersAt(lastListed, now, MEMBER_TTL)
    val plane = servablePlane(DEAD_HEAD_SERVED, deadAt, JOBS.exclude(claimed))
    ms.forall(m =>
      serveSetFor(
        SHARED_HEAD_CONVOY, STEAL_ENABLED, EAGER_STEAL, OWNER_SERVE_SKEW, STEAL_HORIZON,
        m, ms, lastListed, now, plane).forall(j =>
          jobConvertibleAt(deadAt, j) or now - deadAt.get(j) <= STEAL_HORIZON))

  /// bug_083 — the premise-reachability witness for
  /// servedJobsConvertibleWithinHorizon, wired as an EXPECT-VIOLATION
  /// check on the LIVE dead regime (the
  /// quint-matdist-dead-premise-reachable entry in nix/quint.nix —
  /// markers live at the wiring, per the header discipline): VIOLATED
  /// exactly when the law's contended premise — an unclaimed dead job
  /// past the steal-horizon grace, with a live member in the view —
  /// is reached WITHOUT the defect lane's DEAD_HEAD_SERVED plane (the
  /// falsify twin's kill certifies reachability only under that lane,
  /// a strictly weaker proposition). A dieJob guard narrowing or a
  /// DEADABLE shrink that closes the corridor vacuates the law with
  /// the dead-regime holds check still green — THIS check goes red
  /// instead.
  val canReachDeadHeadPastHorizon =
    not(JOBS.exclude(claimed).exists(j => and {
      not(jobConvertibleAt(deadAt, j)),
      now - deadAt.get(j) > STEAL_HORIZON,
      membersAt(lastListed, now, MEMBER_TTL).exists(m => live.get(m)),
    }))
}

/// The wired live regime: horizon 1 < ttl 3 < MAX_TIME 6 keeps the
/// silent-owner gap (stale-but-member) and the TTL re-key both
/// reachable; defect consts all off; the wedge axis OFF (wedgedAt
/// frozen at all-capable — the census is byte-identical to the
/// pre-axis model).
module materializationDistributionBase {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false,
    WEDGE_AXIS_ENABLED = false,
    WEDGED_KEEPS_BEATING = false,
    DEAD_AXIS_ENABLED = false,
    DEAD_HEAD_SERVED = false
  ).*
}

/// The SECOND main (round-6 WO-S1-4): the wedge axis ON in the live
/// law — the honest-beat gate withholds the wedged member's beats,
/// so its slice ages into the steal horizon and re-homes. Bounded
/// exhaustive census: one designated wedgeable member (WEDGEABLE);
/// every other parameter as the base regime.
module materializationDistributionWedge {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false,
    WEDGE_AXIS_ENABLED = true,
    WEDGED_KEEPS_BEATING = false,
    DEAD_AXIS_ENABLED = false,
    DEAD_HEAD_SERVED = false
  ).*
}

/// The THIRD main (round-12 WO-S9-8): the death axis ON in the live
/// law — the node-face exclusion removes a dead job from the
/// servable plane at the listing, so the dead head pins nothing and
/// the convertible remainder keeps draining. Bounded exhaustive
/// census: one designated deadable job (DEADABLE); every other
/// parameter as the base regime (the wedge main's tier precedent —
/// the added axis multiplies the census by the dead states of one
/// job, bounded by MAX_TIME + 2).
module materializationDistributionDead {
  import materializationDistribution(
    MAX_TIME = 6,
    STEAL_HORIZON = 1,
    MEMBER_TTL = 3,
    SHARED_HEAD_CONVOY = false,
    STEAL_ENABLED = true,
    EAGER_STEAL = false,
    OWNER_SERVE_SKEW = false,
    WEDGE_AXIS_ENABLED = false,
    WEDGED_KEEPS_BEATING = false,
    DEAD_AXIS_ENABLED = true,
    DEAD_HEAD_SERVED = false
  ).*
}
