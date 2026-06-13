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
// seconds at 2M/15; the main regime is TLC-exhaustive with the
// episode ghosts FROZEN (TRACK_EPISODE_GHOSTS = false), and the latch
// laws run TLC-exhaustively in wedgeClusterLatch with the ghosts live
// at one-notch-shrunk bounds (the ghost tick-domain product does not
// converge at the main bounds — measured >1.02B states at the 1800s
// budget kill; see the parameter ladder at TRACK_EPISODE_GHOSTS).

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
  // Parameter ladder (state-space control): the two episode ghosts
  // (watermark, expiredSince) multiply the exhaustive space by the
  // tick-domain product of every open expiry run — wiring them
  // always-on into the production regime blew wedge-cluster-main from
  // 206M generated / 1.19M distinct / ~6min to >1.02B generated /
  // >11.3M distinct with a still-growing frontier at the 1800s budget
  // kill (bughunt-3 S7 landing gate, status 124). The ghosts only
  // carry information for the episode-latch laws, so they are gated
  // behind this const: regimes that do not check the latch laws
  // freeze them at their init values (zero state cost; the admission
  // gate passes through, since every run-start > -1) and the latch
  // laws run exhaustively in the dedicated wedgeClusterLatch regime
  // below, at bounds where the ghost product converges.
  const TRACK_EPISODE_GHOSTS: bool
  // merged_bug_034 (Q2 SIGNED, round-4): the trajectory axis. Off ---
  // the retired instantaneous guard (per-tick wedged/population
  // ratio; per-node verdicts fire while a multi-node episode is
  // still forming). On --- the systemic denominator is the
  // REGISTERED fleet (NODES is the model's registered set), the
  // verdict additionally requires breadth (distinct evidence-bearing
  // nodes inside the window) >= BREADTH_MIN (implied at these consts:
  // wedged nodes bear evidence, so breadth >= wedged >= 2 =
  // BREADTH_MIN --- recorded, not load-bearing), and per-node
  // verdicts are WITHHELD while an episode is developing (breadth >=
  // BREADTH_MIN) --- staggered shared-cause onset routes to the
  // systemic arm instead of serial per-node Dead reaps.
  // TB-6 (bughunt-13 referee, SURVIVED, a-1): the round-4 model
  // additionally demanded a PRE-VERDICT ARMING DWELL --- the raw
  // condition holding DWELL_TICKS consecutive ticks before Systemic
  // fired --- which production never exhibits: finalize's ratio arm
  // fires the tick it trips (wedge.rs `systemic = affected >= 2 &&
  // ratio > WEDGE_SYSTEMIC_FRACTION`; the only production dwell is
  // the POST-watermark suppression dwell, WEDGE_VERDICT_DWELL_SECS,
  // modeled as VERDICT_DWELL_TICKS below). The phantom dwell made the
  // model's first tripped tick a developing/retain image where
  // production drains+latches a Systemic --- verdict timing AND
  // effects diverged. The arming dwell (DWELL_TICKS, pendingSince) is
  // DELETED; the trajectory verdict is immediate, like the code.
  const ENABLE_TRAJECTORY: bool
  const BREADTH_MIN: int
  // merged_bug_024 (round-5): the ghost-admission axis. The two
  // consts follow the TRACK_EPISODE_GHOSTS parameter ladder:
  //   TRACK_GHOST_NODES — on: the attribution universe gains one
  //     GHOST node ("g0") OUTSIDE the registered fleet (a node
  //     Karpenter-GC'd before its first observation can still appear
  //     as an open attempt's source_node); off: the universe is the
  //     registered NODES exactly — zero state cost, byte-identical
  //     spaces for every pre-round-5 regime.
  //   ENABLE_ADMISSION — on (the repair): evidence admission consumes
  //     fleet-membership through the per-tick admission authority —
  //     a fleet-absent attribution is refused, so evidence, breadth
  //     and the verdict populations stay within the registered
  //     universe; off (as-built): a ghost is admitted on its first
  //     expiry tick and counted in wedged/breadth/population, minting
  //     false Systemics over a denominator larger than the fleet.
  const TRACK_GHOST_NODES: bool
  const ENABLE_ADMISSION: bool
  // merged_bug_016 (round-5): the suppressed-retention axis. On (the
  // repair): a suppressed-without-drain tick (the engaged breadth
  // phase under ENABLE_TRAJECTORY) RETAINS the marked
  // transition-memory --- the per-arm effects record states
  // marked-retention explicitly instead of feeding an empty survivor
  // set into a shared retain tail. Off (as-built): the developing
  // tick drains marked, so ONE continuous wedge re-counts (and
  // re-warns) on the first post-suppression tick --- the
  // once-per-transition contract breaks with no not-wedged interval.
  // The two carrier vars (lastSuppressedNoDrain, lastShrankMarked)
  // are computed ONLY on suppressed-no-drain ticks (constant false
  // wherever ENABLE_TRAJECTORY is off --- zero state cost; the
  // TRACK_EPISODE_GHOSTS parameter-ladder precedent).
  const ENABLE_SUPPRESSED_RETENTION: bool
  // merged_bug_023 (round-5): the release-edge close. On (the
  // repair): an engaged RETAINING episode (the breadth phase) that
  // disengages on an observed tick CLOSES --- whole-window drain +
  // watermark latch (where the episode ghosts are tracked) + marked
  // drain --- so evidence observed during the engaged episode cannot
  // mint a per-node verdict after release. Off (as-built): the
  // release is a silent third exit --- the retained anchors survive
  // disengagement and a late-onset node Dead-reaps from evidence the
  // suppression itself attributed to a shared cause. The episode
  // machinery (episodeState, the developing-tick suppressedAnchors
  // feed, sawRelease) is gated behind ENABLE_TRAJECTORY --- frozen at
  // init values in every non-trajectory regime (zero state cost; the
  // TRACK_EPISODE_GHOSTS parameter-ladder precedent).
  const ENABLE_RELEASE_CLOSE: bool
  /// merged_bug_023 dwell-engaged arm (bughunt-6 S4): the CODE keeps
  /// an episode engaged while the post-watermark verdict dwell is
  /// active and wedged is non-empty (the Dwell axis); this model
  /// historically CLOSED the moment breadth decayed (releaseEdge) ---
  /// the model was right, the code diverged, and the divergent
  /// trajectory (a Breadth->Dwell downgrade that skips Breadth's
  /// close) was inexpressible. ENABLE_DWELL_ARM=false everywhere
  /// pre-existing = today's model, byte-identical per §4.F16. With
  /// the arm on: engagement = systemic | developing |
  /// (watermark-recent and wedged != {}), episodeState gains the
  /// dwell-engaged value 3, and the release edge at dwell expiry
  /// keys the STORED state --- exactly the code's machine.
  /// DOWNGRADE_RUNS_CLOSE: the law under test --- true = the fix (a
  /// 2->3 downgrade runs the breadth close: drain + merge-latch);
  /// false = the as-built continue-without-effects.
  /// VERDICT_DWELL_TICKS: the post-watermark dwell duration (the
  /// WEDGE_VERDICT_DWELL_SECS abstraction; only read when the arm is
  /// on).
  const ENABLE_DWELL_ARM: bool
  const DOWNGRADE_RUNS_CLOSE: bool
  const VERDICT_DWELL_TICKS: int

  val NODES = Set("n0", "n1", "n2")
  val GHOSTS = if (TRACK_GHOST_NODES) Set("g0") else Set()
  // The attribution universe: every node an open attempt can name.
  val ATTRIB_NODES = NODES.union(GHOSTS)
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
  // m288: suppression-surviving anchors actually observed (the
  // tautology guard for noPerNodeFromSuppressedEvidence).
  var sawSuppressed: bool
  // m288: whether the last verdict-bearing update was a skip (RPC
  // failure) --- the restated counter law exempts unobserved ticks.
  var lastSkip: bool
  // merged_bug_016: the last observed tick was suppressed WITHOUT a
  // drain (developing episode, ratio not fired) --- the retention
  // law's guard.
  var lastSuppressedNoDrain: bool
  // merged_bug_016: on that suppressed-no-drain tick, did any
  // non-evicted marked entry leave the set? (true = the as-built
  // drain; computed only on suppressed-no-drain ticks.)
  var lastShrankMarked: bool
  // merged_bug_023: the engaged-episode phase as of the last observed
  // tick. 0 = disengaged; 1 = engaged via a draining tick (ratio ---
  // its release is a no-op, the window is already drained); 2 =
  // engaged via a retaining tick (breadth/developing --- its release
  // MUST close). Frozen 0 off-trajectory.
  var episodeState: int
  /// bughunt-6 S4 reachability ghost (frozen false when the dwell arm
  /// is off --- zero state cost): a 2->3 (developing->dwell-engaged)
  /// downgrade edge occurred. The dwell-arm hold wires its negation
  /// as an expected-violation check so the hold cannot go vacuous;
  /// the falsify twin's violating trace shows it latched before the
  /// per-node verdict.
  var sawDowngrade: bool
  // merged_bug_023 reachability: a release edge was taken (frozen
  // false off-trajectory).
  var sawRelease: bool

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
    suppressedAnchors' = Set(),
    watermark' = -1,
    expiredSince' = Map(),
    sawSystemic' = false,
    sawPerNode' = false,
    sawSuppressed' = false,
    lastSkip' = false,
    lastSuppressedNoDrain' = false,
    lastShrankMarked' = false,
    episodeState' = 0,
    sawDowngrade' = false,
    sawRelease' = false,
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
      watermark: int, expiredSince: (str, str) -> int,
      suppressedNoDrain: bool, shrankMarked: bool,
      episodeState: int, releaseEdge: bool, downgradeEdge: bool } =
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
    // merged_bug_024 admission authority (fleet-absence leg): a
    // fleet-absent attribution is refused BEFORE the watermark gate —
    // the same gate order as the Rust (authority, then watermark).
    val fleetAdmitted = if (ENABLE_ADMISSION)
        effExpired.filter(p => NODES.contains(p._1))
      else effExpired
    // merged_bug_163 admission gate: an expiry run that began at or
    // before the watermark belongs to a suppressed episode.
    val admitted = if (ENABLE_EPISODE_LATCH)
        fleetAdmitted.filter(p => newExpiredSince.get(p) > watermark)
      else fleetAdmitted
    val evObserved = if (skip) evAfterEvict else pruned(observed(evAfterEvict, admitted, now), now)
    // Under the authority the verdict populations stay within the
    // registered universe (refused ghosts contribute neither evidence
    // nor fleet); as-built the ghost names leak into both.
    val fleet = effHealthy.union(fleetAdmitted.map(p => p._1))
    val wedged = if (skip) Set() else wedgedOf(evObserved, now)
    // merged_bug_034: under the trajectory axis the denominator is
    // the REGISTERED fleet (the model universe), never the per-tick
    // survivor set a traffic lull can shrink.
    val population = if (ENABLE_TRAJECTORY) NODES.size()
      else if (ENABLE_POPULATIONS) evObserved.keys().union(fleet).size()
      else fleet.size()
    val guardOk = if (ENABLE_POPULATIONS or ENABLE_TRAJECTORY) true else fleet.size() > 0
    // Breadth: distinct evidence-bearing nodes inside the window
    // (evObserved is already window-pruned).
    val breadth = evObserved.keys().size()
    val rawSystemic = and {
      not(skip),
      wedged.size() >= 2,
      guardOk,
      // strictly more than half of the population
      2 * wedged.size() > population,
      (not(ENABLE_TRAJECTORY)) or breadth >= BREADTH_MIN,
    }
    // TB-6 (a-1): the verdict is IMMEDIATE --- production's ratio arm
    // fires the tick it trips; the only dwell production exhibits is
    // the POST-watermark suppression dwell (dwellEngaged below). The
    // round-4 arming dwell (raw condition held DWELL_TICKS ticks
    // before firing) was a phantom and is deleted.
    val systemic = rawSystemic
    // merged_bug_034: a developing episode (breadth at the bar while
    // the verdict has not fired) withholds per-node Dead verdicts.
    val developing = ENABLE_TRAJECTORY and breadth >= BREADTH_MIN
    // merged_bug_023: the episode machine. Engagement = a systemic or
    // developing observed tick; the release edge = engaged previously,
    // observed now, disengaged now. Only a RETAINING episode (state 2)
    // closes with effects; ratio episodes (state 1) already drained
    // per-tick and release as a no-op.
    // merged_bug_023 dwell-engaged arm (bughunt-6 S4): the code's
    // third engagement source --- watermark-recent AND wedged
    // non-empty (precedence below ratio/breadth). Requires the
    // watermark ghost; dead when the arm is off.
    val dwellEngaged = and {
      ENABLE_DWELL_ARM,
      ENABLE_TRAJECTORY,
      TRACK_EPISODE_GHOSTS,
      not(skip),
      not(systemic or developing),
      watermark >= 0,
      now - watermark <= VERDICT_DWELL_TICKS,
      wedged.size() > 0,
    }
    val releaseEdge = and {
      ENABLE_TRAJECTORY,
      episodeState != 0,
      not(skip),
      not(systemic or developing or dwellEngaged),
    }
    val closeFires = releaseEdge and (episodeState == 2) and ENABLE_RELEASE_CLOSE
    // bughunt-6 S4: the downgrade edge --- a RETAINING (developing)
    // episode recomputes to dwell-engaged. As-built the stored state
    // silently becomes 3 with NO effects (the defect: the eventual
    // release then keys Dwell's no-op and the retained breadth-phase
    // evidence mints per-node verdicts at dwell expiry); under
    // DOWNGRADE_RUNS_CLOSE the downgrade runs the breadth close
    // (drain + merge-latch) exactly like the release edge.
    val downgradeEdge = (episodeState == 2) and dwellEngaged
    val downgradeFires = downgradeEdge and DOWNGRADE_RUNS_CLOSE
    val newEpisodeState = if (not(ENABLE_TRAJECTORY)) 0
      else if (skip) episodeState
      else if (systemic) 1
      else if (developing) 2
      else if (dwellEngaged) 3
      else 0
    val drained = if (systemic and ENABLE_EPISODE_LATCH)
        // Whole-episode drain: every node in the window.
        Map()
      else if (systemic and ENABLE_FULL_EPILOGUE)
        evObserved.keys().filter(n => not(wedged.contains(n))).mapBy(n => evObserved.get(n))
      else evObserved
    // merged_bug_023: the close drains the WHOLE window at the
    // release edge --- and (bughunt-6 S4) at the DOWNGRADE edge when
    // the law is on.
    val drainedFinal = if (closeFires or downgradeFires) Map() else drained
    val survivors = if (systemic or developing or dwellEngaged or closeFires) Set() else wedged
    // merged_bug_016: a suppressed tick that retains evidence (the
    // developing breadth phase --- ratio did not fire) must retain
    // the marked transition-memory too.
    val suppressedNoDrain = (developing or (dwellEngaged and not(downgradeFires))) and not(systemic) and not(skip)
    // As-built early return: the Systemic arm skips BOTH the insert
    // loop and the retain — marked freezes. m288: a SKIP tick retains
    // the marked set unchanged (the retired encoding wiped it through
    // the empty survivor set — mis-modeling the real fold, which the
    // RPC failure never reaches).
    val newMarked = if (skip)
        markedAfterEvict
      else if (systemic and not(ENABLE_FULL_EPILOGUE))
        markedAfterEvict
      else if (suppressedNoDrain and ENABLE_SUPPRESSED_RETENTION)
        markedAfterEvict
      else if (closeFires or downgradeFires)
        // The close drains the marked transition-memory (the same
        // re-entry law as a ratio drain); the downgrade close
        // (bughunt-6 S4) is the same law at the transition edge.
        Set()
      else survivors
    // Computed only on suppressed-no-drain ticks (zero state cost
    // off-axis): did the tick drop a non-evicted marked entry?
    val shrankMarked = suppressedNoDrain and
      markedAfterEvict.exists(n => not(newMarked.contains(n)))
    val delta = if (skip or (systemic and not(ENABLE_FULL_EPILOGUE)))
        0
      else survivors.exclude(markedAfterEvict).size()
    // Anchors surviving a suppression (per-(node,drv,anchor)
    // identity). m288: fold over the POST-DRAIN map's own keys --- the
    // retired fold iterated WEDGED nodes over `drained`, which
    // excludes them in the epilogue arm and is empty under the latch:
    // provably-empty in every wired regime, leaving
    // noPerNodeFromSuppressedEvidence a tautology.
    val episodeAnchors = if (systemic)
        drainedFinal.keys().fold(Set(), (acc, n) =>
          acc.union(nodeEvidence(drainedFinal, n).keys().map(d => (n, d, nodeEvidence(drainedFinal, n).get(d)))))
      else if (developing and not(skip))
        // merged_bug_023: anchors observed during an ENGAGED
        // retaining episode are episode-explained --- they enter the
        // suppression-survivor set while they remain in evidence, so
        // noPerNodeFromSuppressedEvidence carries the release-edge
        // content: without the close they survive disengagement and
        // can wholly mint a per-node verdict (the twin's violation);
        // with it they are drained at the close and leave the set.
        drainedFinal.keys().fold(Set(), (acc, n) =>
          acc.union(nodeEvidence(drainedFinal, n).keys().map(d => (n, d, nodeEvidence(drainedFinal, n).get(d)))))
      else Set()
    {
      evidence: drainedFinal, marked: newMarked, delta: delta,
      consumed: consumed, systemic: systemic or closeFires,
      affected: if (systemic or closeFires) wedged.size() else 0,
      of_: if (systemic or closeFires) population else 0,
      wedged: if (skip) Set() else survivors,
      skip: skip, episodeAnchors: episodeAnchors,
      // Ghost watermark: latched on a Systemic verdict wherever the
      // ghosts are tracked, so the latch laws are non-vacuous in the
      // twin; only the ADMISSION gate is behind ENABLE_EPISODE_LATCH,
      // and both ghosts freeze at their init values (zero state cost)
      // when TRACK_EPISODE_GHOSTS is off — see the parameter ladder
      // at the const.
      // The close merge-latches the watermark where the episode
      // ghosts are tracked (now >= any prior latch, so assignment IS
      // the merge --- the gate never lowers).
      watermark: if (TRACK_EPISODE_GHOSTS) (if (systemic or closeFires or downgradeFires) now else watermark) else -1,
      expiredSince: if (TRACK_EPISODE_GHOSTS) newExpiredSince else Map(),
      suppressedNoDrain: suppressedNoDrain,
      shrankMarked: shrankMarked,
      episodeState: newEpisodeState,
      releaseEdge: releaseEdge,
      downgradeEdge: downgradeEdge,
    }

  action tick = {
    // Expiries draw from the ATTRIBUTION universe (ghosts included
    // when tracked); healthy attribution stays registered-only — a
    // ghost's signal is exactly its still-open expired attempts.
    nondet expired = tuples(ATTRIB_NODES, DRVS).powerset().oneOf()
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
      sawSuppressed' = (sawSuppressed or r.episodeAnchors.size() > 0),
      lastSkip' = r.skip,
      lastSuppressedNoDrain' = r.suppressedNoDrain,
      lastShrankMarked' = r.shrankMarked,
      episodeState' = r.episodeState,
      sawRelease' = (sawRelease or r.releaseEdge),
      sawDowngrade' = (sawDowngrade or r.downgradeEdge),
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
    suppressedAnchors' = suppressedAnchors,
    watermark' = watermark, expiredSince' = expiredSince,
    sawSystemic' = sawSystemic, sawPerNode' = sawPerNode,
    sawSuppressed' = sawSuppressed, lastSkip' = lastSkip,
    lastSuppressedNoDrain' = lastSuppressedNoDrain, lastShrankMarked' = lastShrankMarked,
    episodeState' = episodeState, sawRelease' = sawRelease,
    sawDowngrade' = sawDowngrade,
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
    // m288 restatement: an unobserved (skip) tick moves the counter
    // by zero and retains the marked set; an observed tick's marked
    // set equals the epilogue's survivor set --- EXCEPT the
    // suppressed-no-drain tick, whose per-variant effects RETAIN the
    // set (merged_bug_016; the retention law below carries that arm).
    lastSkip implies (lastDelta == 0),
    (not(lastSkip) and not(lastSuppressedNoDrain)) implies
      marked == (if (lastSystemic) Set() else lastWedged),
  }
  // merged_bug_016: the retention law --- a suppressed-without-drain
  // tick moves the transition counter by zero and drops no
  // non-evicted marked entry. The as-built shared survivors tail
  // (ENABLE_SUPPRESSED_RETENTION off) violates the second conjunct on
  // the first developing tick after a per-node mark.
  val suppressedTickRetainsMarked = lastSuppressedNoDrain implies and {
    lastDelta == 0,
    not(lastShrankMarked),
  }
  // merged_bug_034 (Q2): the Systemic verdict's denominator is the
  // registered fleet --- a traffic lull (shrunken instantaneous
  // population) can never substitute for it. The instantaneous twin
  // violates this within 3 ticks (lull false-systemic, of_ = 2 over a
  // 3-node fleet).
  val systemicDenominatorIsFleet =
    lastSystemic implies (lastOf == NODES.size())
  // merged_bug_034 (Q2): while a shared-cause episode is developing
  // (>= BREADTH_MIN evidence-bearing nodes in the window), per-node
  // Dead verdicts are withheld --- staggered onset cannot serially
  // reap the fleet below the ratio bar. Post-verdict evidence equals
  // the window's evidence-bearing set on every per-node arm, so the
  // law needs no extra state; the instantaneous twin violates it on
  // the staggered scenario.
  val noSerialReapInDevelopingEpisode =
    (not(lastSystemic)) implies
      (lastWedged.size() == 0 or evidence.keys().size() < BREADTH_MIN)
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

  // merged_bug_024: evidence stays within the registered universe —
  // the admission authority's fleet-absence leg refuses ghost
  // attribution, so no fleet-absent node can accumulate anchors. The
  // as-built (admission off) violates this on a ghost's first
  // observed expiry tick.
  val evidenceWithinRegistered =
    evidence.keys().forall(n => NODES.contains(n))
  // merged_bug_024: the fleet-denominator law over the registered
  // universe — a Systemic verdict's denominator never exceeds the
  // registered fleet (ghosts cannot inflate it; with the lull guard
  // it can still be smaller in non-trajectory regimes). The as-built
  // violates this when admitted ghosts both wedge and pad the
  // population.
  val systemicOfWithinRegisteredUniverse =
    lastSystemic implies (lastOf <= NODES.size())

  val boundsOK = and {
    time >= 0, time <= MAX_TIME,
    markedIncrements >= 0,
  }

  // --- witnesses (negated reachability; wired expect-violation) ---------------
  val systemicReachableW = not(sawSystemic)
  // merged_bug_023 anti-vacuity: the release edge is reachable at the
  // trajectory bounds (an engaged episode disengages on an observed
  // tick).
  val releaseEdgeReachableW = not(sawRelease)
  /// bughunt-6 S4: the downgrade edge is reachable (wired as an
  /// expected-violation check in the dwell-arm hold --- the
  /// noPerNodeFromSuppressedEvidence law must not hold vacuously
  /// over a never-exercised arm).
  val downgradeReachableW = not(sawDowngrade)
  val perNodeReachableW = not(sawPerNode)
  // m288: the suppression-survivor set is actually populated in the
  // wired epilogue regime --- the tautology guard.
  val suppressedReachableW = not(sawSuppressed)
}

// Production-shaped fixed regime: all repairs on. TLC-exhaustive at
// these bounds with the episode ghosts FROZEN — the admission gate
// passes through here, and noRemarkFromLatchedEpisode is checked in
// wedgeClusterLatch instead (wiring the ghosts live at these bounds
// blew past the 1800s budget at >1.02B generated states — bughunt-3
// S7 landing-gate red; see the parameter ladder at
// TRACK_EPISODE_GHOSTS).
module wedgeClusterMain {
  import wedgeCluster(
    MAX_TIME = 5, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = false,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).*
}

// Episode-latch regime: the ghosts run LIVE (admission gating + the
// noRemarkFromLatchedEpisode law) at bounds where the ghost
// tick-domain product converges exhaustively — MAX_TIME/WINDOW shrink
// one notch from the main regime. The latch scenarios (build evidence
// → systemic episode → gated continuation → re-verdict) need 4 ticks
// and a 2-tick window; the systemic-arm reachability witness below
// guards these bounds against silent vacuation. Measured numbers live
// at the quint-wedge-cluster-latch check comment in nix/quint.nix.
module wedgeClusterLatch {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = true,
    ENABLE_TRAJECTORY = false,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
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
    ENABLE_EVICTION = false,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = false,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).*
}

// merged_bug_034 (Q2): the production trajectory regime --- fleet
// denominator + breadth live, per-node withholding during a
// developing episode. One notch below the main bounds (measured
// numbers at the quint-wedge-cluster-trajectory check in
// nix/quint.nix). The systemic-arm witness guards these bounds
// against vacuation (the verdict is IMMEDIATE on the tick the ratio
// trips --- TB-6 deleted the phantom arming dwell; MAX_TIME=4 leaves
// headroom).
module wedgeClusterTrajectory {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = true,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).*
}

// m288: the epilogue regime --- the latch OFF, full epilogue ON: the
// drain leaves bystander evidence, so suppressedAnchors is REACHABLY
// non-empty (suppressedReachableW) and
// noPerNodeFromSuppressedEvidence carries content instead of holding
// over a provably-empty set. This is the law's non-vacuous home; the
// latch regimes above hold it trivially-but-truthfully (whole-episode
// drain leaves no survivors by design).
module wedgeClusterEpilogue {
  import wedgeCluster(
    MAX_TIME = 5, WINDOW = 3, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = false,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = false,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).*
}

// merged_bug_024 (round-5): the ghost-admission regime --- the ghost
// node is live in the attribution universe and the admission
// authority's fleet-absence leg is ON. evidenceWithinRegistered and
// systemicOfWithinRegisteredUniverse carry the repair's content here;
// the as-built twin (calibration/wedge-024-ghost-admission) violates
// both at these exact bounds. Ghost-free regimes above are
// byte-identical to their pre-round-5 spaces (TRACK_GHOST_NODES=false
// keeps ATTRIB_NODES = NODES; the admission filter is an identity
// over registered-only pairs).
module wedgeClusterAdmission {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = false,
    ENABLE_TRAJECTORY = false,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = true,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = false,
    DOWNGRADE_RUNS_CLOSE = false,
    VERDICT_DWELL_TICKS = 1
  ).*
}

// bughunt-6 S4 (merged_bug_023 downgrade law): the dwell-arm HOLD
// regime --- the code's full engagement alphabet (ratio | breadth |
// dwell) with the downgrade close ON. Ghosts on (the dwell reads the
// watermark), trajectory on, latch-regime bounds (MAX_TIME=4,
// WINDOW=2 --- the ghost x trajectory x dwell product; the
// systemic-arm witness guards the bounds against vacuation).
// VERDICT_DWELL_TICKS=2: the dwell-ENGAGED state is cold-reachable
// (a Ratio->Dwell identity transition at t3: post-drain fresh wedged
// evidence under the t2 watermark); the 2->3 DOWNGRADE cell is NOT
// cold-reachable at these bounds (the asymmetric anchor ages need
// the seeded frame --- measured 55M+ states, depth 7, no edge), so
// the downgrade's violation AND reachability halves live in the
// seeded calibration pair
// (calibration/wedge-023-downgrade-skips-close.qnt); the safety law
// (noPerNodeFromSuppressedEvidence under the full alphabet) holds
// exhaustively here.
module wedgeClusterDwellArm {
  import wedgeCluster(
    MAX_TIME = 4, WINDOW = 2, MIN_DISTINCT = 2,
    ENABLE_POPULATIONS = true,
    ENABLE_FULL_EPILOGUE = true,
    ENABLE_EPISODE_LATCH = true,
    ENABLE_EVICTION = true,
    TRACK_EPISODE_GHOSTS = true,
    ENABLE_TRAJECTORY = true,
    BREADTH_MIN = 2,
    TRACK_GHOST_NODES = false,
    ENABLE_ADMISSION = true,
    ENABLE_SUPPRESSED_RETENTION = true,
    ENABLE_RELEASE_CLOSE = true,
    ENABLE_DWELL_ARM = true,
    DOWNGRADE_RUNS_CLOSE = true,
    VERDICT_DWELL_TICKS = 2
  ).*
}
