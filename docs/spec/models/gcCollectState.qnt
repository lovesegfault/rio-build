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

// ===========================================================================
// DRAIN-PASS PLANE (round 3, bug_174 + bug_137 + merged_bug_170): the
// collect pass's COMPLETION law. A pass walks the chunk keyspace in
// key order reaping eligibles; a budget cap persists the cursor and a
// later pass RESUMES from it. In code the law is a TYPE:
// `PassDisposition` (CompleteFullScan | CompleteResumed | Capped) —
// only CompleteFullScan anchors the cycle (backlog-0 claim / reap
// credit); a resumed pass keeps the decremented backlog because keys
// at or below its resume point were never scanned by it, so it cannot
// testify about them (bug_174's understated-backlog counterexample:
// a resumed pass anchoring 0 while below-cursor eligibles exist).
//
// DEDICATED module (S7/TRACK_EPISODE_GHOSTS precedent): the axis adds
// cursor/disposition state that must NOT multiply gcCollectState's
// exhaustive commit-basis regime — it gets its own small regime
// instead of riding always-on in the existing one.
// ===========================================================================
module gcDrainPass {
  /// Chunk keyspace: 1..MAX_CHUNKS, scanned in key order.
  const MAX_CHUNKS: int

  /// CALIBRATION ONLY (`gcDrainPassCalibResumeAnchors`): the pre-fix
  /// wiring — a RESUMED completion also anchors, computing the backlog
  /// claim from the keys IT scanned (above its resume point), blind to
  /// below-cursor eligibles. Both completion laws falsify.
  const CALIB_RESUME_ANCHORS: bool

  val CHUNKS = 1.to(MAX_CHUNKS)

  /// Eligible-to-reap chunks (the cycle mark's product, as writers
  /// move references between and during passes).
  var eligible: Set[int]

  /// The persisted scan position: keys <= cursor have been covered by
  /// the pass chain since the last full-scan open.
  var cursor: int

  /// The open pass's resume point: keys <= passStart were NOT scanned
  /// by THIS pass (0 for a full scan).
  var passStart: int

  /// A pass is open.
  var scanning: bool

  type Origin = | FullScan | Resumed
  var origin: Origin

  /// The last pass-ending event's typed disposition (PassDisposition).
  type Disposition = | DNone | DCompleteFull | DCompleteResumed | DCapped
  var lastPass: Disposition

  /// The cycle anchor: the durable "collection complete, backlog B"
  /// claim, stamped with whether a full scan minted it.
  var anchored: { has: bool, backlog: int, byFull: bool }

  /// Eligibility moved since the anchor was minted (post-mint writer
  /// activity belongs to the NEXT cycle — the truthfulness law is
  /// judged on the mint-clean window, like the commit-basis model's
  /// post-commit guard).
  var dirtySinceMint: bool

  action init = all {
    nondet e = powerset(CHUNKS).oneOf()
    eligible' = e,
    cursor' = 0,
    passStart' = 0,
    scanning' = false,
    origin' = FullScan,
    lastPass' = DNone,
    anchored' = { has: false, backlog: 0, byFull: false },
    dirtySinceMint' = false,
  }

  /// A writer drops a chunk's last reference (it becomes eligible) or
  /// re-references it — between passes or mid-pass, above or below the
  /// cursor. Below-cursor insertions are how a resumed pass comes to
  /// sit above unseen eligibles.
  action mutateEligibility = all {
    nondet h = CHUNKS.oneOf()
    nondet on = Set(true, false).oneOf()
    eligible' = if (on) eligible.union(Set(h)) else eligible.exclude(Set(h)),
    cursor' = cursor,
    passStart' = passStart,
    scanning' = scanning,
    origin' = origin,
    lastPass' = lastPass,
    anchored' = anchored,
    dirtySinceMint' = true,
  }

  /// Open a fresh full-keyspace pass.
  action openFull = all {
    not(scanning),
    scanning' = true,
    cursor' = 0,
    passStart' = 0,
    origin' = FullScan,
    eligible' = eligible,
    lastPass' = lastPass,
    anchored' = anchored,
    dirtySinceMint' = dirtySinceMint,
  }

  /// Resume from the persisted cap cursor: keys <= cursor stay
  /// unscanned by this pass.
  action openResumed = all {
    not(scanning),
    lastPass == DCapped,
    cursor > 0,
    cursor < MAX_CHUNKS,
    scanning' = true,
    cursor' = cursor,
    passStart' = cursor,
    origin' = Resumed,
    eligible' = eligible,
    lastPass' = lastPass,
    anchored' = anchored,
    dirtySinceMint' = dirtySinceMint,
  }

  /// Scan the next key; reap it if eligible.
  action scanNext = all {
    scanning,
    cursor < MAX_CHUNKS,
    cursor' = cursor + 1,
    eligible' = eligible.exclude(Set(cursor + 1)),
    passStart' = passStart,
    scanning' = scanning,
    origin' = origin,
    lastPass' = lastPass,
    anchored' = anchored,
    dirtySinceMint' = dirtySinceMint,
  }

  /// Budget cap mid-scan: persist the cursor, mint DCapped.
  action cap = all {
    scanning,
    cursor < MAX_CHUNKS,
    scanning' = false,
    lastPass' = DCapped,
    cursor' = cursor,
    passStart' = passStart,
    origin' = origin,
    eligible' = eligible,
    anchored' = anchored,
    dirtySinceMint' = dirtySinceMint,
  }

  /// The pass covered the keyspace tail: mint the typed disposition.
  /// REAL wiring: only the full scan anchors (a resumed completion
  /// keeps the decremented backlog and the cycle stays unanchored —
  /// the next full pass earns the anchor). CALIB wiring: the resumed
  /// completion also anchors, from the keys it saw.
  action complete = all {
    scanning,
    cursor == MAX_CHUNKS,
    scanning' = false,
    lastPass' = if (origin == FullScan) DCompleteFull else DCompleteResumed,
    anchored' =
      if (origin == FullScan) {
        { has: true, backlog: eligible.size(), byFull: true }
      } else if (CALIB_RESUME_ANCHORS) {
        { has: true, backlog: eligible.filter(h => h > passStart).size(), byFull: false }
      } else {
        anchored
      },
    dirtySinceMint' =
      if (origin == FullScan or CALIB_RESUME_ANCHORS) false else dirtySinceMint,
    cursor' = cursor,
    passStart' = passStart,
    origin' = origin,
    eligible' = eligible,
  }

  action step = any {
    mutateEligibility,
    openFull,
    openResumed,
    scanNext,
    cap,
    complete,
  }

  // --- invariants (store.gc.completion-witness) ---------------------------

  /// A mint-clean zero-backlog anchor means the keyspace is truly
  /// drained. The calibration twin falsifies it: a resumed pass
  /// anchors 0 while below-resume-point eligibles exist unseen.
  val zeroAnchorTruthful = or {
    not(anchored.has),
    dirtySinceMint,
    anchored.backlog > 0,
    eligible == Set(),
  }

  /// The anchor is mintable ONLY by a full-keyspace scan — the typed
  /// PassDisposition law (CompleteResumed carries no anchor authority).
  val anchoredByFullScanOnly = or {
    not(anchored.has),
    anchored.byFull,
  }

  /// Non-vacuity witness (must VIOLATE in the main regime): a resumed
  /// completion sitting above below-resume-point eligibles is
  /// reachable — the twin's falsification only means something if the
  /// live regime explores exactly that shape.
  val noResumedCompletionWithBelowEligibles = not(and {
    lastPass == DCompleteResumed,
    eligible.filter(h => h <= passStart).size() > 0,
  })
}

// ===========================================================================
// DRAIN-PASS MAIN REGIME: real wiring, both completion laws hold.
// ===========================================================================
module gcDrainPassMain {
  import gcDrainPass(
    MAX_CHUNKS = 4,
    CALIB_RESUME_ANCHORS = false
  ).*

  /// bug_174's counterexample choreography, against the REAL wiring:
  /// reap 1, cap at 1, chunk 1 re-becomes eligible BELOW the cursor,
  /// resume from 1, drain the tail — the resumed completion mints
  /// DCompleteResumed, does NOT anchor, and the below-cursor eligible
  /// survives for the next full pass.
  run resumedCompletionKeepsBacklogRun =
    init
      .then(all {
        eligible' = Set(1, 2),
        cursor' = 0,
        passStart' = 0,
        scanning' = false,
        origin' = FullScan,
        lastPass' = DNone,
        anchored' = { has: false, backlog: 0, byFull: false },
        dirtySinceMint' = false,
      })
      .then(openFull)
      .then(scanNext) // key 1 reaped
      .then(cap) // budget cap at cursor 1
      .expect(all {
        lastPass == DCapped,
        eligible == Set(2),
      })
      .then(all {
        // the writer drops chunk 1's reference again: eligible BELOW
        // the persisted cursor.
        eligible' = eligible.union(Set(1)),
        cursor' = cursor,
        passStart' = passStart,
        scanning' = scanning,
        origin' = origin,
        lastPass' = lastPass,
        anchored' = anchored,
        dirtySinceMint' = true,
      })
      .then(openResumed)
      .then(scanNext) // key 2 reaped
      .then(scanNext) // key 3
      .then(scanNext) // key 4
      .then(complete)
      .expect(all {
        lastPass == DCompleteResumed,
        // the resumed completion anchored NOTHING...
        not(anchored.has),
        // ...and the below-cursor eligible survives for the next full
        // pass (the decremented backlog is kept, never zeroed).
        eligible == Set(1),
        zeroAnchorTruthful,
        anchoredByFullScanOnly,
      })
}

// ===========================================================================
// DRAIN-PASS CALIBRATION TWIN (expect-violation): the resumed
// completion also anchors, from the keys it scanned — both completion
// laws falsify (zero-anchor-over-unseen-eligibles, anchor minted by a
// non-full scan).
// ===========================================================================
module gcDrainPassCalibResumeAnchors {
  import gcDrainPass(
    MAX_CHUNKS = 4,
    CALIB_RESUME_ANCHORS = true
  ).*
}

// ===========================================================================
// BACKLOG-ESTIMATE PLANE (round-13 TB-2 / WO-S9-4): the persisted
// running estimate's three-armed update law — the `backlog_estimate =
// CASE` arm of CycleCommit::Live (rio-store/src/gc/state.rs, the
// execute_commit UPDATE), whose pure LAW OF RECORD is
// rio_store::gc::state::next_backlog_estimate (extracted at this
// close; the SQL stays in the statement for atomicity and names the
// fn). Arms: an anchoring disposition RESETS to zero; a NULL estimate
// SEEDS from the cycle's unmarked-backlog observation minus this
// cycle's victims; an existing estimate RATCHETS down by victims only
// — fresh observations are DISCARDED outside the NULL arm
// (deliberate: bug_174 — a capped/resumed cycle cannot testify about
// keys it never scanned). What the law therefore does NOT claim — and
// the divergence witness + compoundingEstimateRun below DOCUMENT — is
// estimate==truth between anchors: under churn the ratcheted estimate
// diverges (either direction) until the next anchoring full scan, and
// the rio_store_gc_collect_backlog_chunks gauge serves the ESTIMATE.
// This was TB-2's filed core (audit: high, class c — UNREFEREED);
// refereed SURVIVED at round-13: the law had zero model footprint
// across all three gcCollectState planes. It now has exactly this
// one. DEDICATED module (the gcDrainPass precedent): the estimate
// axis must not multiply the drain-pass or commit-basis censuses.
// ===========================================================================
module gcBacklogEstimate {
  /// Truth ceiling (TLC finiteness; the census bound, not a claim).
  const MAX_TRUTH: int
  /// Calibration: the GREATEST floor dropped from both non-anchor
  /// arms (the floor law falsifies on victims past the estimate).
  const CALIB_NO_FLOOR: bool
  /// Calibration: the anchor arm keeps the ratcheted value instead of
  /// resetting (the anchor law falsifies).
  const CALIB_ANCHOR_KEEPS: bool

  /// SQL NULL encoding for the persisted estimate (house int form —
  /// the materializationDistribution deadAt `< 0` precedent).
  val NULL_EST = -1

  pure def maxI(a: int, b: int): int = if (a > b) a else b

  /// Mirrors (the law of record, arm-for-arm):
  /// rio_store::gc::state::next_backlog_estimate — anchor-zero /
  /// seed-when-NULL / ratchet, GREATEST-floored at zero.
  pure def nextEstimate(anchorsZero: bool, current: int, seed: int, victims: int): int =
    if (anchorsZero)
      (if (CALIB_ANCHOR_KEEPS) maxI(current, 0) else 0)
    else if (current == NULL_EST)
      (if (CALIB_NO_FLOOR) seed - victims else maxI(seed - victims, 0))
    else
      (if (CALIB_NO_FLOOR) current - victims else maxI(current - victims, 0))

  /// True eligible backlog (the mark complement's image; churn moves
  /// it between commits).
  var truth: int
  /// The persisted gc_collect_state.backlog_estimate (NULL_EST = SQL
  /// NULL — no observation has ever seeded it).
  var estimate: int
  /// The last commit anchored a zero-backlog claim.
  var lastAnchored: bool

  action init = all {
    truth' = 0,
    estimate' = NULL_EST,
    lastAnchored' = false,
  }

  // Environment: writer activity — references dropped (truth grows)
  // or re-taken (truth shrinks) between cycle commits; the world's
  // move, priced by the MAX_TRUTH census ceiling.
  action churn = all {
    nondet t = 0.to(MAX_TRUTH).oneOf()
    truth' = t,
    estimate' = estimate,
    lastAnchored' = lastAnchored,
  }

  // Mirrors: rio-store/src/gc/state.rs:next_backlog_estimate
  action commitLive = {
    nondet victims = 0.to(truth).oneOf()
    nondet anchors = Set(true, false).oneOf()
    all {
      // An anchoring disposition is a CompleteFullScan: it drained
      // every eligible at its basis (victims == truth);
      // non-anchoring commits (capped/resumed) collect any prefix.
      (not(anchors) or victims == truth),
      estimate' = nextEstimate(anchors, estimate, truth, victims),
      truth' = truth - victims,
      lastAnchored' = anchors,
    }
  }

  action step = any { churn, commitLive }

  /// The floor law (GREATEST(x, 0) in both non-anchor arms): a
  /// persisted estimate is never negative (NULL excepted by
  /// encoding).
  val estimateFloored = estimate == NULL_EST or estimate >= 0

  /// The anchor law: an anchoring commit leaves a zero estimate.
  val anchorZeroResets = not(lastAnchored) or estimate == 0

  /// NON-VACUITY witness (expect-violation): seeding is reachable —
  /// the estimate does not sit NULL forever (the bug_083 pattern).
  val estimateAlwaysNull = estimate == NULL_EST

  /// THE DIVERGENCE witness (expect-violation — the TB-2 compounding
  /// face is REACHABLE under the live law): between anchors the
  /// ratcheted estimate and the truth come apart. A violation here is
  /// a DOCUMENTED divergence of the law, not a falsification of it —
  /// the law never claims tracking; the gauge's reader inherits
  /// exactly this gap.
  val estimateTracksTruth = estimate == NULL_EST or estimate == truth

  /// The compounding face, choreographed (W13-BA): seed at 3−1=2,
  /// churn UP to 4 (the fresh observation the ratchet will discard),
  /// one 2-victim ratchet commit drives the estimate to 0 while the
  /// truth stands at 2 — the silent-zero face: the gauge reads empty
  /// over a live backlog — then the anchoring full drain recovers
  /// agreement.
  run compoundingEstimateRun =
    init
      .then(all {
        // churn: writers drop three references.
        truth' = 3, estimate' = estimate, lastAnchored' = lastAnchored,
      })
      .then(all {
        // Live commit, 1 victim, no anchor: the NULL arm seeds 3−1=2.
        truth' = 2,
        estimate' = nextEstimate(false, estimate, truth, 1),
        lastAnchored' = false,
      })
      .expect(all { estimate == 2, truth == 2, estimateTracksTruth })
      .then(all {
        // churn UP: two more references dropped (truth 4).
        truth' = 4, estimate' = estimate, lastAnchored' = lastAnchored,
      })
      .then(all {
        // ratchet commit, 2 victims: 2−2=0 — the fresh seed (4) is
        // DISCARDED by the ELSE arm; truth stands at 2.
        truth' = 2,
        estimate' = nextEstimate(false, estimate, 4, 2),
        lastAnchored' = false,
      })
      .expect(all {
        estimate == 0,
        truth == 2,
        not(estimateTracksTruth),
        estimateFloored,
      })
      .then(all {
        // the anchoring full drain: victims == truth, zero claimed.
        truth' = 0,
        estimate' = nextEstimate(true, estimate, 2, 2),
        lastAnchored' = true,
      })
      .expect(all { estimate == 0, truth == 0, anchorZeroResets, estimateTracksTruth })
}

/// The live regime (TLC exhaustive at MAX_TRUTH=4 — a small integer
/// lattice; sub-second).
module gcBacklogEstimateMain {
  import gcBacklogEstimate(
    MAX_TRUTH = 4,
    CALIB_NO_FLOOR = false,
    CALIB_ANCHOR_KEEPS = false
  ).*
}

/// CALIBRATION twin (expect-violation): the GREATEST floor dropped —
/// estimateFloored falsifies (victims past the current estimate mint
/// a negative row).
module gcBacklogEstimateCalibNoFloor {
  import gcBacklogEstimate(
    MAX_TRUTH = 4,
    CALIB_NO_FLOOR = true,
    CALIB_ANCHOR_KEEPS = false
  ).*
}

/// CALIBRATION twin (expect-violation): the anchor arm keeps the
/// ratcheted value — anchorZeroResets falsifies on the first
/// anchoring commit over a seeded estimate.
module gcBacklogEstimateCalibAnchorKeeps {
  import gcBacklogEstimate(
    MAX_TRUTH = 4,
    CALIB_NO_FLOOR = false,
    CALIB_ANCHOR_KEEPS = true
  ).*
}
