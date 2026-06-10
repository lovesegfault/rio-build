// -*- mode: Bluespec; -*-
//
// live_047/R-C WO-R7-4: the materialization walk's path-axis evidence
// law under bounded fan-out (`store.materialize.path-fold`) — ONE walk
// iteration's window/latch/fold plane, with quint's nondeterministic
// scheduling as the completion-order adversary.
//
// SCOPE (stated per the design §5.4): all four holds are PER-WALK
// SAFETY properties over one walk's completed evidence —
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
// viable at F=2 × 4 paths.
//
// Path kinds are static (one per PathResolution class; sizes = path
// id), which covers the charge+transient interleaving the
// arrival-pick twin needs:
//   1 = Served(size 1), 2 = Settled, 3 = AbortGrade(charge),
//   4 = AbortGrade(transient).
//
// Twins (each dies through exactly one hold; kill isolation for the
// arrival-pick twin is its own holds lane on the window leaves):
//   calibration/walkfanout-arrival-pick      -> foldTierLawful
//   calibration/walkfanout-spawn-after-abort -> noSpawnAfterLatch
//   calibration/walkfanout-spawn-unbounded   -> windowWithinF
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
  }

  action step = any {
    spawn,
    complete,
    cancelSibling,
    reResolve,
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
}
