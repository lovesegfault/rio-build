// gcCollectState.qnt — the collect cycle's COMMIT BASIS law
// (bughunt-2 slot 7, bug_226; `rio-store/src/gc/collect.rs`
// `DurableObservation` / `CycleCommit`).
//
// WHAT THIS MODELS. One axis of the collect cycle, deliberately
// standalone (chunkCollect.qnt models the sweep/reap interleavings;
// importing this axis there would fan a new variable through ~30
// frame disciplines for no interleaving value — the basis law is a
// per-commit functional property, not a concurrency property): the
// cycle observes the chunk population under a REPEATABLE READ
// snapshot, optionally simulates an operator exclusion list
// (`simulated_swept`, the bug_199 preview lane), and COMMITS one
// durable observation row (`gc_collect_state`: live count + backlog).
//
// THE LAW (store.gc.observation-basis): the committed observation
// anchors the REAL basis — what is live/eligible under NO exclusions
// at the commit snapshot. The simulated preview is a reporting lane
// (GcStats); committing it would publish a counterfactual: live-count
// too small, backlog too large, and every downstream pacing/alerting
// consumer of `gc_collect_state` acts on a world that does not exist.
// In code the law is a TYPE: `DurableObservation`'s constructor is
// private to the real-basis arm (`from_real_basis`), and
// `CycleCommit::{Shadow,Live}` accept nothing else. Here the law is
// the pair of invariants the calibration twin falsifies.
//
// PARAMETER LADDER. MAX_CHUNKS=3 / MAX_MANIFESTS=2 explores every
// reference shape (unreferenced, singly, multiply, excluded-only
// referenced); the state space is the cross product of reference maps
// x exclusion sets x snapshot copies x phase — ~4.2e6 states, TLC
// minutes. The first wiring used 4/3 AND carried the snapshot map
// inside `published`, squaring the map dimension into ~1e11+ states:
// the TLC checks ran 14+ hours without a verdict and wedged a wave
// gate. Measured runtime at this regime is recorded in the
// introducing commit; the nix constructors now hard-bound every
// checker's wall clock so a regression of this class is a RED check,
// not an eternal gate.

module gcCollectState {
  // --- constants ---------------------------------------------------------

  /// Chunk ids: 1..MAX_CHUNKS.
  const MAX_CHUNKS: int

  /// Manifest ids: 1..MAX_MANIFESTS. A manifest is this model's unit
  /// of reference AND of exclusion (production excludes upstreams; an
  /// upstream's manifests are what the exclusion removes from the
  /// mark's reference walk).
  const MAX_MANIFESTS: int

  /// CALIBRATION ONLY (`gcCollectStateCalibSimBasis`): the pre-fix
  /// shadow-commit wiring — the durable row is built from the
  /// SIMULATED (exclusion-applied) products instead of the real ones
  /// (collect.rs pre-C2: `CycleCommit::Shadow` carried the preview's
  /// counts). Both basis invariants falsify.
  const CALIB_COMMIT_SIM_BASIS: bool

  val CHUNKS = 1.to(MAX_CHUNKS)
  val MANIFESTS = 1.to(MAX_MANIFESTS)

  // --- state -------------------------------------------------------------

  /// Which manifests reference each chunk (the manifest_data walk's
  /// projection). Mutable between cycles: writers add/drop references.
  var refsOf: int -> Set[int]

  /// The operator exclusion list for the NEXT cycle (the dry-run
  /// what-if: "what would the mark look like without these
  /// manifests?").
  var excluded: Set[int]

  /// Cycle phase: idle, or snapshot pinned (the REPEATABLE READ tx is
  /// open and `snapRefs` is the world it sees).
  type Phase = | PIdle | PSnapped

  var phase: Phase

  /// The pinned snapshot of `refsOf` (REPEATABLE READ: concurrent
  /// `mutate` steps do not move it).
  var snapRefs: int -> Set[int]

  /// The exclusion list as the snapshot captured it.
  var snapExcluded: Set[int]

  /// The durable `gc_collect_state` row: the last committed
  /// observation. The snapshot it was derived from is `snapRefs` —
  /// which the next `snapshot` action overwrites, so the basis
  /// invariants are guarded on the post-commit window (`phase ==
  /// PIdle`): between a commit and the next snapshot, `snapRefs` IS
  /// the basis the row was obliged to come from (production carries
  /// the same pairing implicitly — the row is written inside the
  /// snapshot's transaction). Carrying the snapshot INSIDE this record
  /// instead squares the reachable-state count by the map domain and
  /// put the TLC checks past any wall clock (the wave's 14-hour wedge;
  /// see the regime doc).
  var published: { live: int, backlog: int, committed: bool }

  // --- derived -----------------------------------------------------------

  /// Real basis: live under NO exclusions.
  def realLive(rm: int -> Set[int]): Set[int] = CHUNKS.filter(h => rm.get(h) != Set())

  /// Real backlog: eligible-to-collect (unreferenced) under NO
  /// exclusions.
  def realBacklog(rm: int -> Set[int]): Set[int] = CHUNKS.filter(h => rm.get(h) == Set())

  /// Simulated basis: live after the exclusion list strips its
  /// manifests from every reference set (the preview lane).
  def simLive(rm: int -> Set[int], ex: Set[int]): Set[int] =
    CHUNKS.filter(h => rm.get(h).exclude(ex) != Set())

  def simBacklog(rm: int -> Set[int], ex: Set[int]): Set[int] =
    CHUNKS.filter(h => rm.get(h).exclude(ex) == Set())

  // --- actions -----------------------------------------------------------

  action init = all {
    nondet rm = CHUNKS.setOfMaps(powerset(MANIFESTS)).oneOf()
    refsOf' = rm,
    nondet ex = powerset(MANIFESTS).oneOf()
    excluded' = ex,
    phase' = PIdle,
    snapRefs' = CHUNKS.mapBy(h => Set()),
    snapExcluded' = Set(),
    published' = { live: 0, backlog: 0, committed: false },
  }

  /// A writer moves a reference (upload, path sweep, re-reference)
  /// between cycles or DURING an open snapshot — the snapshot must not
  /// care (REPEATABLE READ).
  action mutate = all {
    nondet h = CHUNKS.oneOf()
    nondet rs = powerset(MANIFESTS).oneOf()
    refsOf' = refsOf.set(h, rs),
    excluded' = excluded,
    phase' = phase,
    snapRefs' = snapRefs,
    snapExcluded' = snapExcluded,
    published' = published,
  }

  /// The operator edits the exclusion list (between snapshots only —
  /// production reads it once at cycle start).
  action setExclusions = all {
    phase == PIdle,
    nondet ex = powerset(MANIFESTS).oneOf()
    excluded' = ex,
    refsOf' = refsOf,
    phase' = phase,
    snapRefs' = snapRefs,
    snapExcluded' = snapExcluded,
    published' = published,
  }

  /// Open the cycle's REPEATABLE READ snapshot: pin the reference
  /// world and the exclusion list.
  action snapshot = all {
    phase == PIdle,
    phase' = PSnapped,
    snapRefs' = refsOf,
    snapExcluded' = excluded,
    refsOf' = refsOf,
    excluded' = excluded,
    published' = published,
  }

  /// Commit the cycle's durable observation (`CycleCommit::Shadow`
  /// with a non-empty exclusion list is the interesting arm: the
  /// preview simulated, the commit must NOT). The live arm
  /// (`CycleCommit::Live`) is the degenerate empty-exclusion case of
  /// the same law. CALIB_COMMIT_SIM_BASIS re-wires the commit to the
  /// simulated products — the pre-fix bug, and the only difference
  /// between the main modules and the calibration twin.
  action commitObservation = all {
    phase == PSnapped,
    phase' = PIdle,
    published' =
      if (CALIB_COMMIT_SIM_BASIS) {
        live: simLive(snapRefs, snapExcluded).size(),
        backlog: simBacklog(snapRefs, snapExcluded).size(),
        committed: true,
      } else {
        live: realLive(snapRefs).size(),
        backlog: realBacklog(snapRefs).size(),
        committed: true,
      },
    refsOf' = refsOf,
    excluded' = excluded,
    snapRefs' = snapRefs,
    snapExcluded' = snapExcluded,
  }

  action step = any {
    mutate,
    setExclusions,
    snapshot,
    commitObservation,
  }

  // --- invariants (store.gc.observation-basis) ---------------------------

  /// The committed live count IS the real mark count over the
  /// snapshot it was derived from — never the exclusion-shrunk
  /// preview. Enforced on the post-commit window (`phase == PIdle`),
  /// where `snapRefs` still holds the commit's own snapshot; a newly
  /// opened snapshot (PSnapped) suspends the comparison until its own
  /// commit rewrites `published`.
  val publishedLiveEqualsRealMark = or {
    not(published.committed),
    phase == PSnapped,
    published.live == realLive(snapRefs).size(),
  }

  /// The committed backlog never exceeds the true eligible count over
  /// the same snapshot (the simulated lane can only INFLATE backlog —
  /// an exclusion strips references, making more chunks look
  /// collectable than are). Same post-commit-window guard.
  val backlogLeTrueEligible = or {
    not(published.committed),
    phase == PSnapped,
    published.backlog <= realBacklog(snapRefs).size(),
  }

  /// A committed observation with a non-empty effective exclusion is
  /// reachable — the non-vacuity witness: the calibration twin's
  /// falsification only means something if the live modules actually
  /// explore the shadow-with-exclusions arm. (`noEffectiveShadowCommit`
  /// must VIOLATE in the main regime.)
  val noEffectiveShadowCommit = not(and {
    published.committed,
    phase == PIdle,
    simLive(snapRefs, snapExcluded).size() != realLive(snapRefs).size(),
  })
}

// ===========================================================================
// MAIN REGIME: full reference-shape cross product, exclusions on, the
// fixed commit wiring. Both basis laws hold.
// ===========================================================================
module gcCollectStateMain {
  import gcCollectState(
    MAX_CHUNKS = 3,
    MAX_MANIFESTS = 2,
    CALIB_COMMIT_SIM_BASIS = false
  ).*

  /// The operator dry-run anchor (mirrors collect.rs
  /// `dry_run_commit_anchors_real_basis`): chunk 1 referenced only by
  /// excluded manifest 1, chunk 2 by manifest 2 — the preview says
  /// live=1/backlog=...; the COMMIT publishes the real basis.
  run dryRunCommitAnchorsRealBasisRun =
    init
      .then(all {
        refsOf' = CHUNKS.mapBy(h => if (h == 1) Set(1) else if (h == 2) Set(2) else Set()),
        excluded' = Set(1),
        phase' = PIdle,
        snapRefs' = snapRefs,
        snapExcluded' = snapExcluded,
        published' = published,
      })
      .then(snapshot)
      .expect(all {
        // the preview lane simulates: chunk 1 reads dead under the
        // exclusion...
        simLive(snapRefs, snapExcluded) == Set(2),
        simBacklog(snapRefs, snapExcluded).contains(1),
        // ...while the real basis keeps it live.
        realLive(snapRefs) == Set(1, 2),
      })
      .then(commitObservation)
      .expect(all {
        published.committed,
        published.live == 2,
        published.backlog == MAX_CHUNKS - 2,
        publishedLiveEqualsRealMark,
        backlogLeTrueEligible,
      })
}

// ===========================================================================
// CALIBRATION TWIN (expect-violation): the pre-fix commit publishes the
// SIMULATED basis. Identical regime, exactly one constant flipped —
// both basis invariants falsify, machine-checked evidence the
// invariants police the wiring they were written for.
// ===========================================================================
module gcCollectStateCalibSimBasis {
  import gcCollectState(
    MAX_CHUNKS = 3,
    MAX_MANIFESTS = 2,
    CALIB_COMMIT_SIM_BASIS = true
  ).*
}
