// -*- mode: Bluespec; -*-
//
// live_047/R-C WO-R7-4: the materialization walk's path-axis evidence
// law under bounded fan-out (`store.materialize.path-fold+1`) — ONE walk
// iteration's window/latch/fold plane, with quint's nondeterministic
// scheduling as the completion-order adversary.
//
// SCOPE (stated per the design §5.4): all holds are PER-WALK SAFETY
// properties over one walk's completed evidence —
// `foldTierLawful` checks every schedule including abort-first (the
// model-level complement of the W-1c Rust witness; the post-latch
// `complete` branch IS the dequeue-backlog family, `cancelSibling` the
// other branch, so the §2.3 window-membership nondeterminism is
// explored exhaustively). The cross-attempt charge-latency direction
// is deliberately OUT of model scope and carries no witness anywhere
// (accepted on the §2.3 derivation). NOT an extension of
// materializationJob.qnt: its slot-10 walk-fold plane models ONE fold
// evaluation's cell laws; grafting a cross-path interleaving axis onto
// the 3.6k-line model would blast its const-binding radius across the
// calibration corpus — a small dedicated model keeps exhaustive TLC
// viable at F=2 × 4 paths. The bug_102 slot-then-claim ordering law
// (slot ≺ claim ≺ admission ≺ budget) is likewise OUT of model scope:
// this file models one iteration's window/latch/fold plane and has no
// claim concept — the ordering is pinned by the Rust `ClaimAdmission`
// type and its reds.
//
// SECOND MAIN (merged_bug_012): `walkFanoutProgress`, below in this
// file, models the driver-owned window-progress fold on a dedicated
// two-path plane — the per-path streamed counters stay OUT of this
// base main's census (the round-6 second-main rule pre-applied).
//
// Path kinds are static (one per PathResolution class; sizes = path
// id), which covers the charge+transient interleaving the
// arrival-pick twin needs:
//   1 = Served(size 1), 2 = Settled, 3 = AbortGrade(charge),
//   4 = AbortGrade(transient).
//
// Twins (each dies through exactly one hold; kill isolation for the
// arrival-pick and dup-frontier twins is their own holds lane on the
// window leaves):
//   calibration/walkfanout-arrival-pick          -> foldTierLawful
//   calibration/walkfanout-spawn-after-abort     -> noSpawnAfterLatch
//   calibration/walkfanout-spawn-unbounded       -> windowWithinF
//   calibration/walkfanout-dup-frontier          -> frontierSpawnable
//   calibration/walkfanout-progress-base-capture -> noFalseCompletion
//                                       (+ its own emittedDoneNeverRegresses lane)
//
// State counts, depth, and wall-clock: in the wired check's
// transcript and the introducing commit message, never here.
module walkFanout {
  /// The per-job window width under test (helm default is 4; F=2 with
  /// 4 paths exhibits every law — window contention, the backlog
  /// family, charge+transient interleavings — at exhaustive-TLC cost).
  pure val F = 2
  /// One path per evidence kind.
  pure val PATHS = Set(1, 2, 3, 4)
  /// Generation bumps per trace (law 5 needs exactly one boundary to
  /// be non-vacuous; more only squares the space).
  pure val MAX_GEN = 1

  pure def isServed(p: int): bool = p == 1
  pure def isSettled(p: int): bool = p == 2
  pure def isCharge(p: int): bool = p == 3
  pure def isTransient(p: int): bool = p == 4
  pure def narSize(p: int): int = p

  /// Frontier paths not yet spawned.
  var pending: Set[int]
  /// The in-flight window (≤ F — the law-2 bound).
  var inflight: Set[int]
  /// Completed Served paths (the floor's only source).
  var served: Set[int]
  /// Generation-stamped settled cells (law 3 / bug_266's fold_guard
  /// premise).
  var settledCells: Set[{ path: int, g: int }]
  /// COMPLETED abort-grade evidence — the tier fold's input multiset.
  var abortsDone: Set[int]
  /// Cancelled-by-drop siblings: zero cells, zero floor trace.
  var cancelled: Set[int]
  /// The abort latch (law 4): no spawns once set.
  var latched: bool
  /// The committed progress floor.
  var floor: int
  /// Tenant-set generation (law 5: bumps only with the window drained).
  var gen: int
  /// "" until the outcome compiles; then "infra" | "retry".
  var verdict: str
  /// Ghost: the FIRST abort-grade completion's kind — the
  /// arrival-pick twin's substrate (unused by the live fold).
  var firstAbortVerdict: str
  /// Ghost: a spawn happened while latched — only the
  /// spawn-after-abort twin's perturbed seat can set it.
  var spawnedPostLatch: bool
  /// Paths ADMITTED to the frontier (merged_bug_003: `visited` is
  /// marked at ENQUEUE). At this main's geometry every path is
  /// admitted at init, so the set is constant — its role is the
  /// reEnqueue seat's live gate.
  var visitedM: Set[int]
  /// Ghost: duplicate frontier entries currently queued — entries
  /// that justify a slot acquire but spawn nothing. Only the
  /// dup-frontier twin's perturbed gate can raise it (the live gate
  /// drops re-offers of visited paths at the door).
  var dupQueued: int

  action init = all {
    pending' = PATHS,
    inflight' = Set(),
    served' = Set(),
    settledCells' = Set(),
    abortsDone' = Set(),
    cancelled' = Set(),
    latched' = false,
    floor' = 0,
    gen' = 0,
    verdict' = "",
    firstAbortVerdict' = "",
    spawnedPostLatch' = false,
    visitedM' = PATHS,
    dupQueued' = 0,
  }

  /// Law-2 spawn SEAT: `ignoreLatch` is the decision — the live walk
  /// never spawns past the latch (false); the spawn-after-abort twin
  /// flips it. The ghost records a latched spawn (reachable only
  /// under the perturbation).
  action spawnSeat(ignoreLatch: bool): bool = {
    nondet p = oneOf(pending)
    all {
      verdict == "",
      ignoreLatch or not(latched),
      inflight.size() < F,
      pending' = pending.exclude(Set(p)),
      inflight' = inflight.union(Set(p)),
      spawnedPostLatch' = (spawnedPostLatch or latched),
      served' = served,
      settledCells' = settledCells,
      abortsDone' = abortsDone,
      cancelled' = cancelled,
      latched' = latched,
      floor' = floor,
      gen' = gen,
      verdict' = verdict,
      firstAbortVerdict' = firstAbortVerdict,
      visitedM' = visitedM,
      dupQueued' = dupQueued,
    }
  }
  action spawn = spawnSeat(false)

  /// Completion-order apply (law 3): WHICH in-flight path completes
  /// is the adversary's pick. A post-latch complete is the dequeue
  /// BACKLOG (a sibling that finished before the drain collected it);
  /// `cancelSibling` is the other branch — together they explore the
  /// §2.3 window-membership nondeterminism exhaustively.
  action complete = {
    nondet p = oneOf(inflight)
    all {
      verdict == "",
      inflight' = inflight.exclude(Set(p)),
      pending' = pending,
      served' = if (isServed(p)) served.union(Set(p)) else served,
      // bug_087: only the success witness raises the floor.
      floor' = if (isServed(p)) floor + narSize(p) else floor,
      settledCells' = if (isSettled(p))
        settledCells.union(Set({ path: p, g: gen }))
      else settledCells,
      abortsDone' = if (isCharge(p) or isTransient(p))
        abortsDone.union(Set(p))
      else abortsDone,
      latched' = (latched or isCharge(p) or isTransient(p)),
      firstAbortVerdict' = if ((isCharge(p) or isTransient(p)) and firstAbortVerdict == "")
        (if (isCharge(p)) "infra" else "retry")
      else firstAbortVerdict,
      cancelled' = cancelled,
      gen' = gen,
      verdict' = verdict,
      spawnedPostLatch' = spawnedPostLatch,
      visitedM' = visitedM,
      dupQueued' = dupQueued,
    }
  }

  /// Law-4 cancellation-by-drop: post-latch only; a cancelled sibling
  /// contributes zero cells and zero floor trace.
  action cancelSibling = {
    nondet p = oneOf(inflight)
    all {
      latched,
      verdict == "",
      inflight' = inflight.exclude(Set(p)),
      cancelled' = cancelled.union(Set(p)),
      pending' = pending,
      served' = served,
      settledCells' = settledCells,
      abortsDone' = abortsDone,
      latched' = latched,
      floor' = floor,
      gen' = gen,
      verdict' = verdict,
      firstAbortVerdict' = firstAbortVerdict,
      spawnedPostLatch' = spawnedPostLatch,
      visitedM' = visitedM,
      dupQueued' = dupQueued,
    }
  }

  /// Law-5 generation barrier: the tenant re-resolve happens ONLY at
  /// the iteration boundary (window AND frontier drained, no latch).
  /// The bump drains settled cells back to pending (bug_266 growth —
  /// drain_stale), so surviving cells always carry the current
  /// generation.
  action reResolve = all {
    inflight == Set(),
    pending == Set(),
    not(latched),
    verdict == "",
    gen < MAX_GEN,
    gen' = gen + 1,
    pending' = settledCells.map(c => c.path),
    settledCells' = Set(),
    inflight' = inflight,
    served' = served,
    abortsDone' = abortsDone,
    cancelled' = cancelled,
    latched' = latched,
    floor' = floor,
    verdict' = verdict,
    firstAbortVerdict' = firstAbortVerdict,
    spawnedPostLatch' = spawnedPostLatch,
    visitedM' = visitedM,
    dupQueued' = dupQueued,
  }

  /// Law 4's fold: the outcome compiles from the COMPLETED
  /// abort-evidence multiset by the kernel tier law — charge
  /// dominates transient; arrival order is invisible.
  action compileOutcome = all {
    latched,
    inflight == Set(),
    verdict == "",
    verdict' = if (abortsDone.exists(p => isCharge(p))) "infra" else "retry",
    pending' = pending,
    inflight' = inflight,
    served' = served,
    settledCells' = settledCells,
    abortsDone' = abortsDone,
    cancelled' = cancelled,
    latched' = latched,
    floor' = floor,
    gen' = gen,
    firstAbortVerdict' = firstAbortVerdict,
    spawnedPostLatch' = spawnedPostLatch,
    visitedM' = visitedM,
    dupQueued' = dupQueued,
  }

  /// Terminal stutter so completed walks are not TLC deadlocks.
  action idle = all {
    pending' = pending,
    inflight' = inflight,
    served' = served,
    settledCells' = settledCells,
    abortsDone' = abortsDone,
    cancelled' = cancelled,
    latched' = latched,
    floor' = floor,
    gen' = gen,
    verdict' = verdict,
    firstAbortVerdict' = firstAbortVerdict,
    spawnedPostLatch' = spawnedPostLatch,
    visitedM' = visitedM,
    dupQueued' = dupQueued,
  }

  /// merged_bug_003 enqueue-dedup SEAT: a completion re-offers an
  /// already-admitted path (the closure-diamond apply push / the
  /// cross-chain new_seeds×reseed arrival). The LIVE gate is
  /// membership-in-visitedM — marked at ENQUEUE, so the re-offer is
  /// dropped at the door and frontier membership keeps witnessing
  /// spawnable work. The dup-frontier twin's gate is
  /// containment-in-PENDING (the two-timestamp shape: an
  /// already-SPAWNED path is no longer pending, so its duplicate
  /// LANDS as a spawnable-work lie). Dups are modeled as the
  /// dupQueued ghost only — they justify a slot acquire but spawn
  /// nothing, so no other state component moves. The ghost SATURATES
  /// at 1: one dup suffices to witness the membership lie, and the
  /// bound keeps the twin's exhaustive-TLC space finite (the live
  /// gate never raises it at all).
  action reEnqueueSeat(dedupAtEnqueue: bool): bool = {
    nondet p = oneOf(visitedM)
    all {
      verdict == "",
      not(latched),
      dupQueued' = if (dedupAtEnqueue or pending.contains(p)) dupQueued else 1,
      pending' = pending,
      inflight' = inflight,
      served' = served,
      settledCells' = settledCells,
      abortsDone' = abortsDone,
      cancelled' = cancelled,
      latched' = latched,
      floor' = floor,
      gen' = gen,
      verdict' = verdict,
      firstAbortVerdict' = firstAbortVerdict,
      spawnedPostLatch' = spawnedPostLatch,
      visitedM' = visitedM,
    }
  }
  action reEnqueue = reEnqueueSeat(true)

  action step = any {
    spawn,
    complete,
    cancelSibling,
    reResolve,
    reEnqueue,
    compileOutcome,
    idle,
  }

  // ── holds ─────────────────────────────────────────────────────────
  // (named single-conjunct leaves per the quint-policy P7 discipline —
  // each owes its own falsify twin)

  /// Law 2, width clause: in-flight ≤ F at every instant (the
  /// spawn-unbounded twin's kill surface).
  val windowWithinF = inflight.size() <= F

  /// Law 2, latch clause: zero spawns after the latch (the ghost —
  /// the spawn-after-abort twin's kill surface).
  val noSpawnAfterLatch = not(spawnedPostLatch)

  /// Law 4: the job verdict is the tier fold of COMPLETED evidence —
  /// a function of the multiset, never of arrival order within it.
  /// Checked over all schedules including abort-first (W-1c's
  /// model-level complement: with the charge sibling cancelled, the
  /// completed multiset holds only the transient and "retry" is the
  /// LAWFUL verdict).
  val foldTierLawful = or {
    verdict == "",
    and { verdict == "infra", abortsDone.exists(p => isCharge(p)) },
    and {
      verdict == "retry",
      not(abortsDone.exists(p => isCharge(p))),
      abortsDone.exists(p => isTransient(p)),
    },
  }

  /// Laws 3+6 floor clause: the committed floor equals the sum of
  /// served sizes — only the success witness raises it; cancelled,
  /// settled, and aborted paths leave zero floor trace.
  val floorMonotoneCommitOnly =
    floor == served.fold(0, (acc, p) => acc + narSize(p))

  /// Law 5: every surviving settled cell carries the CURRENT
  /// generation (fold_guard's premise) — the window drains before any
  /// re-resolve and the bump drains stale cells.
  val cellsSingleGeneration = settledCells.forall(c => c.g == gen)

  /// merged_bug_003: frontier membership WITNESSES spawnable work —
  /// no duplicate entry is ever queued (the enqueue-dedup gate), so
  /// `!frontier.is_empty()` never justifies a blocking slot acquire
  /// for zero spawnable work (the dup-frontier twin's kill surface).
  val frontierSpawnable = (dupQueued == 0)
}

/// merged_bug_012: the driver-owned window-progress fold on a
/// dedicated two-path plane (Served sizes 2 and 3 — the
/// floor-overtakes-declared cell is reachable by construction: commit
/// the size-3 path while the size-2 path still streams; a size-1
/// sibling would complete on its first tick and never stream UNDER an
/// overtaken floor — the recorded divergence from the book's {1,3}
/// prescription). SUCCESS-ONLY
/// scope, stated per the val docs: the abort/cancel plane lives in the
/// base main; the licensed step-back after a FAILED attempt is the
/// Rust witness plane's W-012c. ALL emissions — stream ticks AND
/// commit frames — are in the ghosts' quantification: commits emit
/// the window aggregate (the e2e wire-monotone law; the floor-only
/// commit shape was machine-refuted by the substitute-progress-e2e
/// VM assertion at width > 1).
module walkFanoutProgress {
  /// Declared sizes (narinfo NarSize): path 1 small, path 2 large.
  pure val SIZES = Map(1 -> 2, 2 -> 3)
  pure val PPATHS = Set(1, 2)

  /// Frontier not yet spawned.
  var pendingP: Set[int]
  /// In-flight window (F = 2 — both may stream concurrently).
  var inflightP: Set[int]
  /// Per-path streamed counter of the CURRENT fetch.
  var streamedM: int -> int
  /// Per-path floor captured at SPAWN — the base-capture twin's
  /// substrate (unused by the live fold).
  var baseM: int -> int
  /// The committed progress floor (success witnesses only).
  var floorP: int
  /// Last PROVISIONAL emission (the fold's wire pair); valid once
  /// provValid.
  var provDone: int
  var provExpected: int
  var provValid: bool
  /// Outstanding declared work over the window AT the last
  /// provisional emission (couples the hold to the emission instant).
  var provOutstandingAtEmit: int
  /// Ghost: the provisional done sequence regressed (the oscillation
  /// cell — only the base-capture twin's emission seat can set it).
  var lastProvisionalDone: int
  var regressed: bool

  action init = all {
    pendingP' = PPATHS,
    inflightP' = Set(),
    streamedM' = PPATHS.mapBy(p => 0),
    baseM' = PPATHS.mapBy(p => 0),
    floorP' = 0,
    provDone' = 0,
    provExpected' = 0,
    provValid' = false,
    provOutstandingAtEmit' = 0,
    lastProvisionalDone' = 0,
    regressed' = false,
  }

  action spawnP = {
    nondet p = oneOf(pendingP)
    all {
      pendingP' = pendingP.exclude(Set(p)),
      inflightP' = inflightP.union(Set(p)),
      // The twin's substrate: capture the floor at spawn.
      baseM' = baseM.set(p, floorP),
      streamedM' = streamedM,
      floorP' = floorP,
      provDone' = provDone,
      provExpected' = provExpected,
      provValid' = provValid,
      provOutstandingAtEmit' = provOutstandingAtEmit,
      lastProvisionalDone' = lastProvisionalDone,
      regressed' = regressed,
    }
  }

  /// One streamed byte + the PROVISIONAL emission. The SEAT is the
  /// emission arithmetic: the live driver fold emits
  /// (floor + Σ streamed, floor + Σ declared) over the window; the
  /// base-capture twin emits the clamped spawn-base pair
  /// (max(floor, base+streamed), max(base+declared, done')) — the
  /// pre-fix future-side shape.
  action streamSeat(rawBase: bool): bool = {
    nondet p = oneOf(inflightP.filter(q => streamedM.get(q) < SIZES.get(q)))
    val streamed2 = streamedM.set(p, streamedM.get(p) + 1)
    val sumStreamed = inflightP.fold(0, (acc, q) => acc + streamed2.get(q))
    val sumDeclared = inflightP.fold(0, (acc, q) => acc + SIZES.get(q))
    val doneLive = floorP + sumStreamed
    val expectedLive = floorP + sumDeclared
    val doneTwinRaw = baseM.get(p) + streamed2.get(p)
    val doneTwin = if (doneTwinRaw > floorP) doneTwinRaw else floorP
    val expectedTwinRaw = baseM.get(p) + SIZES.get(p)
    val expectedTwin = if (expectedTwinRaw > doneTwin) expectedTwinRaw else doneTwin
    val emittedDone = if (rawBase) doneTwin else doneLive
    val emittedExpected = if (rawBase) expectedTwin else expectedLive
    all {
      streamedM' = streamed2,
      provDone' = emittedDone,
      provExpected' = emittedExpected,
      provValid' = true,
      provOutstandingAtEmit' = (sumDeclared - sumStreamed),
      regressed' = (regressed or (provValid and (emittedDone < lastProvisionalDone))),
      lastProvisionalDone' = emittedDone,
      pendingP' = pendingP,
      inflightP' = inflightP,
      baseM' = baseM,
      floorP' = floorP,
    }
  }
  action streamTick = streamSeat(false)

  /// The success-witness commit (apply boundary): the floor absorbs
  /// the path's full size, the path leaves the window, and the frame
  /// EMITS THROUGH THE FOLD — `(floor' + Σ streamed, floor' + Σ
  /// declared)` over the post-retire window (the e2e wire-monotone
  /// law: a floor-only frame regressed the wire at width > 1 — the
  /// machine-refuted divergence (b); empty window ⇒ (floor', floor'),
  /// the F = 1 shape). Commit frames are IN the emission ghosts'
  /// quantification.
  action commitP = {
    nondet p = oneOf(inflightP.filter(q => streamedM.get(q) == SIZES.get(q)))
    val inflight2 = inflightP.exclude(Set(p))
    val floor2 = floorP + SIZES.get(p)
    val sumStreamed2 = inflight2.fold(0, (acc, q) => acc + streamedM.get(q))
    val sumDeclared2 = inflight2.fold(0, (acc, q) => acc + SIZES.get(q))
    val emittedDone = floor2 + sumStreamed2
    val emittedExpected = floor2 + sumDeclared2
    all {
      inflightP' = inflight2,
      floorP' = floor2,
      provDone' = emittedDone,
      provExpected' = emittedExpected,
      provValid' = true,
      provOutstandingAtEmit' = (sumDeclared2 - sumStreamed2),
      regressed' = (regressed or (provValid and (emittedDone < lastProvisionalDone))),
      lastProvisionalDone' = emittedDone,
      pendingP' = pendingP,
      streamedM' = streamedM,
      baseM' = baseM,
    }
  }

  action idleP = all {
    pendingP' = pendingP,
    inflightP' = inflightP,
    streamedM' = streamedM,
    baseM' = baseM,
    floorP' = floorP,
    provDone' = provDone,
    provExpected' = provExpected,
    provValid' = provValid,
    provOutstandingAtEmit' = provOutstandingAtEmit,
    lastProvisionalDone' = lastProvisionalDone,
    regressed' = regressed,
  }

  action step = any {
    spawnP,
    streamTick,
    commitP,
    idleP,
  }

  // ── holds (P7 single-proposition leaves; each owes a falsify lane
  // in calibration/walkfanout-progress-base-capture.qnt) ─────────────

  /// merged_bug_012 / W-012a's model half: NO emission (stream tick
  /// OR commit frame) renders complete while the window holds
  /// outstanding declared work — done == expected only when
  /// Σ streamed == Σ declared at the emission instant (success-only
  /// trace scope).
  val noFalseCompletion = or {
    not(provValid),
    provOutstandingAtEmit == 0,
    provDone < provExpected,
  }

  /// merged_bug_012 / W-012b's model half (the oscillation leaf): the
  /// emitted done sequence — stream ticks AND commit frames — never
  /// regresses on a success-only trace: cross-sibling interleave
  /// cannot move the wire pair backwards and an apply boundary cannot
  /// dip it (the e2e resProgress-monotone law).
  val emittedDoneNeverRegresses = not(regressed)
}
