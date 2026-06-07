// -*- mode: Bluespec; -*-
//
// Model of the controller's OA2 wedge-clustering verdict after the
// bughunt-2 slot-5 rework (merged_bug_009 + merged_bug_176):
// `rio-controller/src/reconcilers/nodeclaim_pool/wedge.rs` —
// WedgePopulations::from_window (the commensurable numerator/
// denominator constructor), WedgeTracker::finalize (the sealed single
// exit running the full epilogue on BOTH arms), the required eviction
// argument fed from health::ReapOutcome.reaped_nodes, and the
// None-view RPC-failure skip.
//
// WHAT THIS MODELS
//   - The per-tick verdict fold over an environment-chosen open-attempt
//     view: which (node, drv) expiry pairs are observed and which
//     nodes are healthily attributed (the fleet), or an RPC failure
//     (no view at all). Evidence anchors at FIRST observation and ages
//     out of the window; ≤3 nodes / 2 drvs keep the space tiny.
//   - The three repair axes, each a const so the as-built defect is
//     expressible and twin-falsified:
//       ENABLE_POPULATIONS  — on: denominator = evidence-nodes ∪ fleet
//         (wedged ⊆ evidence-nodes ⊆ population, so affected ≤ of BY
//         CONSTRUCTION) and an RPC-failed tick skips observation AND
//         verdict; off (as-built): denominator = this-tick fleet only,
//         guarded by the !fleet.is_empty() escape, and an RPC failure
//         runs the verdict over an empty view — the mass-Dead-reap
//         polarity.
//       ENABLE_FULL_EPILOGUE — on: the Systemic arm drains the
//         suppressed nodes' evidence and re-derives the marked set
//         (zero new state; undiminished episodes re-suppress on fresh
//         anchors, §5-Q21); off (as-built): early return past the
//         epilogue — marked freezes, evidence survives suppression.
//       ENABLE_EVICTION — on: backing nodes the controller reaped are
//         evicted from the window before the next verdict (the
//         REQUIRED argument); off (as-built): no eviction input
//         existed — a reaped node's stale evidence keeps feeding the
//         Dead arm and inflating the populations.
//
// INVARIANTS (all live-computed against the last verdict; the flag
// vars are written by the same action that computes the verdict —
// every one is paired with a twin that violates it):
//   affectedLeOf                    — a Systemic verdict's populations
//                                     are commensurable.
//   noPerNodeFromSuppressedEvidence — a per-node verdict never marks a
//                                     node from anchors that survived a
//                                     suppressed episode (drain makes
//                                     this structural).
//   reapedImpliesEvicted            — consumed evictions leave no
//                                     evidence behind.
//   markedIncrementsOnlyOnEdges     — the transition counter moves by
//                                     exactly |newly-marked| per tick
//                                     and the marked set equals the
//                                     epilogue's survivor set (stale
//                                     marked entries would silently
//                                     swallow genuine re-wedge edges).
//
// TWINS (calibration/, live-import form):
//   wedge-009-split-population — ENABLE_POPULATIONS off: violates
//     affectedLeOf within 2 ticks (Systemic{affected:2, of:1}).
//   wedge-176-early-return     — ENABLE_FULL_EPILOGUE off: violates
//     markedIncrementsOnlyOnEdges / noPerNodeFromSuppressedEvidence at
//     depth 4 (suppression freezes marked; retained anchors mass-mark).
//
// Budgets: ≤3 nodes × 2 drvs × small clock — every check converges in
// seconds at 2M/15; the main regime is TLC-exhaustive.

module wedgeCluster {
  // --- bounds & knobs ------------------------------------------------------
  const MAX_TIME: int            // bounded clock (ticks)
  const WINDOW: int              // evidence window (ticks)
  const MIN_DISTINCT: int        // distinct drvs to wedge a node (prod 2)
  const ENABLE_POPULATIONS: bool
  const ENABLE_FULL_EPILOGUE: bool
  const ENABLE_EVICTION: bool
  // Round-3 repair axis (merged_bug_163): on — the Systemic arm
  // drains the WHOLE episode's evidence and evidence admission must
  // beat the suppression watermark (an expired attempt contributes
  // only when its expiry run began strictly after the last
  // suppression); off (round-2 as-built) — wedged-only drain, no
  // admission gate: the episode's still-open attempts re-anchor at
  // the trailing edge and sub-threshold participants' anchors pair
  // with any later blip.
  const ENABLE_EPISODE_LATCH: bool

  val NODES = Set("n0", "n1", "n2")
  val DRVS = Set("d0", "d1")

  // --- state ----------------------------------------------------------------
  var time: int
  // node -> drv -> first-observation anchor (the window anchor).
  var evidence: str -> (str -> int)
  var marked: Set[str]
  var markedIncrements: int       // the rio_controller_node_wedge_marked_total mirror
  var pendingEvictions: Set[str]  // reaped backing nodes awaiting consumption
  // Ghost: every reap with its timestamp — written by reapMarked, read
  // by reapedImpliesEvicted (anchors predating a reap must not survive
  // the next update).
  var reapHistory: Set[(str, int)]
  // --- last-verdict image (live-computed by update, read by invariants) ---
  var lastSystemic: bool
  var lastAffected: int
  var lastOf: int
  var lastWedged: Set[str]
  var lastDelta: int              // counter increments this update
  var lastConsumedEvictions: Set[str]
  // Anchors that survived a suppressed episode: (node, drv, anchor).
  // Written by the same update that suppresses; drained anchors leave.
  var suppressedAnchors: Set[(str, str, int)]
  // Ghost (always tracked, both regimes): the last Systemic verdict's
  // tick — the suppression watermark. -1 = never suppressed.
  var watermark: int
  // Ghost (always tracked): per (node, drv) pair, the tick its
  // CURRENT continuous expiry run began (a pair absent for a tick
  // closed its attempt; a later re-appearance is a NEW expiry).
  var expiredSince: (str, str) -> int
  // reachability evidence
  var sawSystemic: bool
  var sawPerNode: bool
  var sawReWedge: bool            // a node marked, unmarked, marked again

  // --- helpers ---------------------------------------------------------------
  def inWindow(anchor: int, now: int): bool = now - anchor <= WINDOW

  def nodeEvidence(ev: str -> (str -> int), n: str): str -> int =
    if (ev.keys().contains(n)) ev.get(n) else Map()

  def wedgedOf(ev: str -> (str -> int), now: int): Set[str] =
    ev.keys().filter(n =>
      nodeEvidence(ev, n).keys().filter(d => inWindow(nodeEvidence(ev, n).get(d), now)).size()
        >= MIN_DISTINCT
    )

  def pruned(ev: str -> (str -> int), now: int): str -> (str -> int) =
    ev.keys()
      .filter(n => nodeEvidence(ev, n).keys().exists(d => inWindow(nodeEvidence(ev, n).get(d), now)))
      .mapBy(n =>
        nodeEvidence(ev, n)
          .keys()
          .filter(d => inWindow(nodeEvidence(ev, n).get(d), now))
          .mapBy(d => nodeEvidence(ev, n).get(d))
      )

  // Fold one observed view into evidence (first-observation anchors).
  def observed(ev: str -> (str -> int), expired: Set[(str, str)], now: int): str -> (str -> int) =
    expired.fold(ev, (acc, p) =>
      val n = p._1
      val d = p._2
      val cur = nodeEvidence(acc, n)
      val withAnchor = if (cur.keys().contains(d)) cur else cur.put(d, now)
      acc.put(n, withAnchor)
    )

  // --- init ---------------------------------------------------------------
  action init = all {
    time' = 0,
    evidence' = Map(),
    marked' = Set(),
    markedIncrements' = 0,
    pendingEvictions' = Set(),
    reapHistory' = Set(),
    lastSystemic' = false,
    lastAffected' = 0,
    lastOf' = 0,
    lastWedged' = Set(),
    lastDelta' = 0,
    lastConsumedEvictions' = Set(),
    suppressedAnchors' = Set(),
    watermark' = -1,
    expiredSince' = Map(),
    sawSystemic' = false,
    sawPerNode' = false,
    sawReWedge' = false,
  }

  // --- one reconcile tick: evict, observe (or skip), prune, finalize ----------
  // `expired` ⊆ NODES×DRVS is the tick's deadline-expired build
  // attempts; `healthy` ⊆ NODES the additionally-attributed healthy
  // nodes; together they form the tick's fleet. rpcFail models the
  // ListOpenAttempts error (view = None). The whole fold is one pure
  // def returning the next-state record (flat assignments below keep
  // Apalache's assignment finder happy).
  def foldTick(expired: Set[(str, str)], healthy: Set[str], rpcFail: bool):
    { evidence: str -> (str -> int), marked: Set[str], delta: int,
      consumed: Set[str], systemic: bool, affected: int, of_: int,
      wedged: Set[str], skip: bool, episodeAnchors: Set[(str, str, int)],
      watermark: int, expiredSince: (str, str) -> int } =
    val now = time + 1
    val consumed = if (ENABLE_EVICTION) pendingEvictions else Set()
    val evAfterEvict = evidence.keys().filter(n => not(consumed.contains(n))).mapBy(n => evidence.get(n))
    val markedAfterEvict = marked.exclude(consumed)
    // As-built RPC failure = empty view through the full fold; fixed = skip.
    val skip = rpcFail and ENABLE_POPULATIONS
    val effExpired = if (rpcFail) Set() else expired
    val effHealthy = if (rpcFail) Set() else healthy
    // Ghost: continuous expiry runs. An unobserved tick freezes the
    // ghost (attempts did not close because we failed to look).
    val newExpiredSince = if (rpcFail) expiredSince else
      effExpired.mapBy(p => if (expiredSince.keys().contains(p)) expiredSince.get(p) else now)
    // merged_bug_163 admission gate: an expiry run that began at or
    // before the watermark belongs to a suppressed episode.
    val admitted = if (ENABLE_EPISODE_LATCH)
        effExpired.filter(p => newExpiredSince.get(p) > watermark)
      else effExpired
    val evObserved = if (skip) evAfterEvict else pruned(observed(evAfterEvict, admitted, now), now)
    val fleet = effHealthy.union(effExpired.map(p => p._1))
    val wedged = if (skip) Set() else wedgedOf(evObserved, now)
    val population = if (ENABLE_POPULATIONS) evObserved.keys().union(fleet).size() else fleet.size()
    val guardOk = if (ENABLE_POPULATIONS) true else fleet.size() > 0
    val systemic = and {
      not(skip),
      wedged.size() >= 2,
      guardOk,
      // strictly more than half of the population
      2 * wedged.size() > population,
    }
    val drained = if (systemic and ENABLE_EPISODE_LATCH)
        // Whole-episode drain: every node in the window.
        Map()
      else if (systemic and ENABLE_FULL_EPILOGUE)
        evObserved.keys().filter(n => not(wedged.contains(n))).mapBy(n => evObserved.get(n))
      else evObserved
    val survivors = if (systemic) Set() else wedged
    // As-built early return: the Systemic arm skips BOTH the insert
    // loop and the retain — marked freezes.
    val newMarked = if (systemic and not(ENABLE_FULL_EPILOGUE))
        markedAfterEvict
      else survivors
    val delta = if (systemic and not(ENABLE_FULL_EPILOGUE))
        0
      else survivors.exclude(markedAfterEvict).size()
    // Anchors surviving a suppression (per-(node,drv,anchor) identity).
    val episodeAnchors = if (systemic)
        wedged.fold(Set(), (acc, n) =>
          acc.union(nodeEvidence(drained, n).keys().map(d => (n, d, nodeEvidence(drained, n).get(d)))))
      else Set()
    {
      evidence: drained, marked: newMarked, delta: delta,
      consumed: consumed, systemic: systemic,
      affected: if (systemic) wedged.size() else 0,
      of_: if (systemic) population else 0,
      wedged: if (skip) Set() else survivors,
      skip: skip, episodeAnchors: episodeAnchors,
      // Ghost watermark: ALWAYS latched on a Systemic verdict (both
      // regimes), so the invariant is non-vacuous in the twin; only
      // the ADMISSION gate is behind ENABLE_EPISODE_LATCH.
      watermark: if (systemic) now else watermark,
      expiredSince: newExpiredSince,
    }

  action tick = {
    nondet expired = tuples(NODES, DRVS).powerset().oneOf()
    nondet healthy = NODES.powerset().oneOf()
    nondet rpcFail = Set(true, false).oneOf()
    val r = foldTick(expired, healthy, rpcFail)
    all {
      time < MAX_TIME,
      time' = time + 1,
      evidence' = r.evidence,
      marked' = r.marked,
      markedIncrements' = markedIncrements + r.delta,
      pendingEvictions' = pendingEvictions.exclude(r.consumed),
      reapHistory' = reapHistory,
      lastSystemic' = r.systemic,
      lastAffected' = r.affected,
      lastOf' = r.of_,
      lastWedged' = r.wedged,
      lastDelta' = r.delta,
      lastConsumedEvictions' = r.consumed,
      // Episode survivors accumulate; an anchor leaves the set when it
      // no longer exists in evidence (re-anchored fresh or drained).
      suppressedAnchors' = suppressedAnchors
        .union(r.episodeAnchors)
        .filter(a => and {
          r.evidence.keys().contains(a._1),
          nodeEvidence(r.evidence, a._1).keys().contains(a._2),
          nodeEvidence(r.evidence, a._1).get(a._2) == a._3,
        }),
      watermark' = r.watermark,
      expiredSince' = r.expiredSince,
      sawSystemic' = (sawSystemic or r.systemic),
      sawPerNode' = (sawPerNode or (not(r.systemic) and r.wedged.size() > 0)),
      sawReWedge' = (sawReWedge or (r.delta > 0 and markedIncrements > 0 and marked.size() == 0)),
    }
  }

  // The controller reaps a currently-marked node (Dead arm took it):
  // its backing-node name lands in the eviction stash for the NEXT
  // update — exactly mod.rs's pending_wedge_evictions flow.
  action reapMarked = all {
    marked.size() > 0,
    nondet n = marked.oneOf()
    all {
      pendingEvictions' = pendingEvictions.union(Set(n)),
      reapHistory' = reapHistory.union(Set((n, time))),
    },
    time' = time, evidence' = evidence, marked' = marked,
    markedIncrements' = markedIncrements,
    lastSystemic' = lastSystemic, lastAffected' = lastAffected, lastOf' = lastOf,
    lastWedged' = lastWedged, lastDelta' = lastDelta,
    lastConsumedEvictions' = lastConsumedEvictions,
    suppressedAnchors' = suppressedAnchors,
    watermark' = watermark, expiredSince' = expiredSince,
    sawSystemic' = sawSystemic, sawPerNode' = sawPerNode, sawReWedge' = sawReWedge,
  }

  // (nn): the literal step alias — runs live in regime modules only.
  action step = any { tick, reapMarked }

  // --- invariants ------------------------------------------------------------
  // merged_bug_009: Systemic populations are commensurable.
  val affectedLeOf = lastSystemic implies (lastAffected <= lastOf)
  // Drain-on-suppression makes per-node verdicts from episode-surviving
  // anchors structurally impossible: a marked node's qualifying
  // evidence can never be entirely suppression-survivors.
  val noPerNodeFromSuppressedEvidence =
    (not(lastSystemic)) implies
      lastWedged.forall(n =>
        not(
          nodeEvidence(evidence, n).keys().size() > 0 and
          nodeEvidence(evidence, n).keys().forall(d =>
            suppressedAnchors.contains((n, d, nodeEvidence(evidence, n).get(d))))
        )
      )
  // Reap feedback is consumed by the NEXT update: once an update has
  // run after a reap (reap time < time), no anchor predating that reap
  // survives for the reaped node — re-detection from FRESH expiries is
  // by design; re-marking from retained pre-reap evidence is the
  // defect. The as-built (no eviction input) violates this on the
  // first post-reap tick.
  val reapedImpliesEvicted =
    reapHistory.forall(r =>
      (r._2 < time) implies
        nodeEvidence(evidence, r._1).keys().forall(d =>
          nodeEvidence(evidence, r._1).get(d) > r._2))
  // The transition counter moves by exactly |newly-marked| and the
  // marked set equals the epilogue's survivor set — a frozen marked
  // set (early return) breaks the second conjunct on the suppression
  // tick and silently swallows the next genuine edge's increment.
  val markedIncrementsOnlyOnEdges = and {
    lastDelta >= 0,
    marked == (if (lastSystemic) Set() else lastWedged.union(marked.intersect(lastWedged))),
  }
  // merged_bug_163: a per-node verdict never marks a node whose
  // qualifying evidence stems from a suppressed episode — every
  // anchor of a wedged node maps to an expiry RUN that began strictly
  // after the watermark. The latch makes this structural (admission
  // gating + whole-episode drain); the round-2 as-built regime
  // (latch off) violates it on both holes: the trailing-edge laggard
  // (same still-open attempts re-anchor post-drain) and the
  // sub-threshold participant (surviving episode anchor pairs with a
  // later blip).
  val noRemarkFromLatchedEpisode =
    (not(lastSystemic) and watermark >= 0) implies
      lastWedged.forall(n =>
        nodeEvidence(evidence, n).keys().forall(d =>
          expiredSince.keys().contains((n, d)) implies
            expiredSince.get((n, d)) > watermark))

  val boundsOK = and {
    time >= 0, time <= MAX_TIME,
    markedIncrements >= 0,
  }

  // --- witnesses (negated reachability; wired expect-violation) ---------------
  val systemicReachableW = not(sawSystemic)
  val perNodeReachableW = not(sawPerNode)
}

// Production-shaped fixed regime: all repairs on. TLC-exhaustive at
// these bounds.
module wedgeClusterMain {
  import wedgeCluster(
    MAX_TIME = 5, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true
  ).*
}

// As-built eviction gap (kept in-file: the eviction repair is the
// REQUIRED-argument shape — its absence violates reapedImpliesEvicted
// in 3 ticks: mark, reap, re-verdict from retained evidence).
module wedgeClusterNoEviction {
  import wedgeCluster(
    MAX_TIME = 5, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = false
  ).*
}
