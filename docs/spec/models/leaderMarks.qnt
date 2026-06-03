// -*- mode: Bluespec; -*-
//
// The leader-marks reconciliation protocol of rio-lease
// (spawn_patch_leader_marks / sweep_peer_leader_marks /
// spawn_verify_leader_marks): the deletion-cost annotation + leader
// label a leading pod keeps converged on itself, sweeps off stale
// peers, and — since bughunt-wave F1 (merged_bug_138) — periodically
// VERIFIES against its own stored pod state.
//
// Spec rules verified here (markers at the nix/quint.nix wiring):
//   - sched.lease.marks-verify (the bounded-cadence verification: ANY
//     marks falsifier is re-discovered within one verify interval)
//   - sched.lease.deletion-cost+3 (the holder-aware sweep: the peer
//     sweep spares the current Lease holder)
//
// SCOPE, deliberately narrow: two pods, one reconciler ("us" runs the
// modeled machinery; the peer's own loop is environment), an abstract
// holder (who the Lease names), an external falsifier budget (the
// strip nobody's edge writers see: a foreign sweep racing a
// re-acquire, kubectl, any future actor), and the single-flight
// reconcile/verify task. Time is the reconciler's round counter.
//
// THE INVARIANT (marksDivergenceBounded): whenever OUR pod's stored
// marks diverge from our leadership, the divergence is already
// discovered (dirty, or a task in flight) or younger than the verify
// cadence. Pre-fix (edge-triggered marks, the
// lease-138-edge-only calibration) an external strip aged unboundedly.
//
// wrongSince DISCIPLINE (review note from the wave specs): the stamp
// is a single derived helper applied in EVERY action's frame — there
// is no enumerated writer list to fall out of sync with, which is
// what keeps the green check non-vacuous.
//
// State counts, depth, and wall-clock: in the wired check's transcript
// and the introducing commit message, never here.
module leaderMarks {
  /// The verify cadence in reconciler rounds (production:
  /// MARKS_VERIFY_EVERY = 12 renews; the model uses a small value —
  /// the property is cadence-relative).
  const VERIFY_EVERY: int
  /// Round ceiling (state-space bound).
  const MAX_TIME: int
  /// External strips per trace (the falsifier budget; 0 = disabled).
  const MAX_STRIPS: int
  /// Holder changes per trace (state-space bound).
  const MAX_HOLDER_CHANGES: int

  type Pod = str
  /// Who the Lease names. None = vacant.
  type HolderState = | NoLeader | Leader(Pod)
  /// The single-flight reconcile/verify slot: at most one in flight,
  /// carrying the polarity captured at spawn and — for the sweep —
  /// the holder read in the same pass (the holder-aware sweep,
  /// rider R2 of merged_bug_138).
  type TaskState = | NoTask | Reconcile({ polarity: bool, sweepHolder: HolderState })

  val PODS: Set[Pod] = Set("us", "other")

  // --- state -------------------------------------------------------------
  /// The Lease's holder (abstract; drives desire).
  var holder: HolderState
  /// Stored leader marks per pod (true = leader marks present).
  var marks: Pod -> bool
  /// OUR reconciler's marks_dirty flag.
  var dirty: bool
  /// The single-flight task.
  var task: TaskState
  /// OUR reconciler's round counter.
  var clock: int
  /// Round of the last verify pass.
  var lastVerify: int
  /// Per-pod divergence stamp: the round the pod's marks first
  /// diverged from desire; -1 = converged. Maintained by ONE derived
  /// helper in every action (see stampWrong).
  var wrongSince: Pod -> int
  /// External strips so far.
  var strips: int
  /// Holder changes so far.
  var holderChanges: int

  // --- desire / divergence ------------------------------------------------
  def leadingOf(p: Pod, h: HolderState): bool = h == Leader(p)
  /// Desired marks = leadership (cost 1 + label iff leading).
  def wrongUnder(m: Pod -> bool, h: HolderState, p: Pod): bool =
    m.get(p) != leadingOf(p, h)

  /// THE single divergence stamp, applied in every action's frame:
  /// newly-wrong stamps now, still-wrong keeps its stamp, converged
  /// clears. No enumerated writer list.
  def stampWrong(m: Pod -> bool, h: HolderState, ws: Pod -> int, now: int): Pod -> int =
    PODS.mapBy(p =>
      if (wrongUnder(m, h, p)) (if (ws.get(p) == -1) now else ws.get(p))
      else -1)

  // --- invariants / witnesses ---------------------------------------------
  /// Bounds tripwire.
  val boundsOK = and {
    clock <= MAX_TIME,
    strips <= MAX_STRIPS,
    holderChanges <= MAX_HOLDER_CHANGES,
  }

  /// The headline: OUR pod's divergence is discovered (dirty / task in
  /// flight) or younger than the verify cadence. The repair itself is
  /// the next reconcile (level-triggered; pinned by the production
  /// loop tests) — the model pins DISCOVERY boundedness, which is what
  /// the pre-fix machinery lacked.
  val marksDivergenceBounded =
    wrongUnder(marks, holder, "us") implies or {
      dirty,
      task != NoTask,
      clock - wrongSince.get("us") <= VERIFY_EVERY,
    }

  // The holder-aware sweep half (deletion-cost+3) is encoded
  // structurally in reconcileComplete (the sweep skips sweepHolder,
  // captured in the same pass) and pinned at the production level by
  // the red-first unit peer_sweep_spares_current_lease_holder — NOT by
  // a model invariant: the spawn-to-complete TOCTOU (a holder change
  // between the capture and the strip) makes "the holder is never
  // swept" deliberately false, and that residual is exactly what the
  // verify cadence bounds (the victim's own loop re-discovers within
  // VERIFY_EVERY — marksDivergenceBounded from ITS perspective). A
  // vacuous "holds by construction" val here would be decoration.

  /// Witness (expect-violation): the external strip actually fires —
  /// the green marksDivergenceBounded verdict is about a space where
  /// the falsifier bites.
  val noStrip = strips == 0

  // --- actions --------------------------------------------------------
  /// One reconciler round passes. The verify-cadence cap (the
  /// loopInterval pattern of leaderElection.qnt): a leading, clean,
  /// idle reconciler cannot advance past the cadence without the
  /// verify pass having run — the production loop runs verify every
  /// VERIFY_EVERY rounds by construction.
  action tickRound = all {
    clock < MAX_TIME,
    (leadingOf("us", holder) and not(dirty) and task == NoTask) implies
      (clock + 1 - lastVerify <= VERIFY_EVERY),
    clock' = clock + 1,
    holder' = holder, marks' = marks, dirty' = dirty, task' = task,
    lastVerify' = lastVerify, strips' = strips,
    holderChanges' = holderChanges,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
  }

  /// The Lease changes hands (steal, graceful release + steal, the
  /// environment's election). OUR edge writers fire: any change of
  /// our own leadership dirties the marks (the production
  /// acquire/lose/rebound edges).
  action holderChange(h: HolderState): bool = all {
    holderChanges < MAX_HOLDER_CHANGES,
    h != holder,
    holder' = h,
    dirty' = (dirty or (leadingOf("us", h) != leadingOf("us", holder))),
    marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips,
    holderChanges' = holderChanges + 1,
    wrongSince' = stampWrong(marks, h, wrongSince, clock),
  }

  /// THE FALSIFIER: an external actor strips a pod's marks — a foreign
  /// sweep racing a re-acquire (the read-then-patch TOCTOU), kubectl,
  /// any future actor. NO local flag moves: the whole point is that
  /// the edge-writer enumeration does not see it.
  action externalStrip(p: Pod): bool = all {
    strips < MAX_STRIPS,
    marks.get(p),
    marks' = marks.put(p, false),
    strips' = strips + 1,
    holder' = holder, dirty' = dirty, task' = task, clock' = clock,
    lastVerify' = lastVerify, holderChanges' = holderChanges,
    wrongSince' = stampWrong(marks.put(p, false), holder, wrongSince, clock),
  }

  /// The bounded-cadence verify pass (sched.lease.marks-verify): a
  /// leading, clean, idle reconciler reads its OWN pod and re-dirties
  /// on divergence. Enabled any round (the cadence cap on tickRound
  /// FORCES it within VERIFY_EVERY; firing earlier is sound).
  action verify = all {
    leadingOf("us", holder),
    not(dirty),
    task == NoTask,
    lastVerify' = clock,
    dirty' = wrongUnder(marks, holder, "us"),
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    strips' = strips, holderChanges' = holderChanges,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
  }

  /// The reconcile spawns: dirty + free slot. Captures the polarity
  /// AND the holder (the same-pass Lease read of the holder-aware
  /// sweep — rider R2).
  action reconcileSpawn = all {
    dirty,
    task == NoTask,
    task' = Reconcile({ polarity: leadingOf("us", holder), sweepHolder: holder }),
    holder' = holder, marks' = marks, dirty' = dirty, clock' = clock,
    lastVerify' = lastVerify, strips' = strips,
    holderChanges' = holderChanges,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
  }

  /// Field accessors for the in-flight task (-defaults for NoTask;
  /// every consumer guards task != NoTask first — the leaderElection
  /// accessor convention).
  def taskPolarity: bool = match task { | NoTask => false | Reconcile(r) => r.polarity }
  def taskSweepHolder: HolderState = match task { | NoTask => NoLeader | Reconcile(r) => r.sweepHolder }

  /// The reconcile completes: own marks written at the captured
  /// polarity; while leading, the sweep strips every OTHER labeled pod
  /// EXCEPT the captured holder (sched.lease.deletion-cost+3 — the
  /// just-re-acquired peer is spared). The flag clears only when the
  /// captured polarity still matches the desire (the production
  /// stale-polarity re-dirty).
  action reconcileComplete = {
    val newMarks = PODS.mapBy(p =>
      if (p == "us") taskPolarity
      else if (taskPolarity and marks.get(p) and Leader(p) != taskSweepHolder) false
      else marks.get(p))
    all {
      task != NoTask,
      marks' = newMarks,
      task' = NoTask,
      dirty' = (taskPolarity != leadingOf("us", holder)),
      holder' = holder, clock' = clock, lastVerify' = lastVerify,
      strips' = strips, holderChanges' = holderChanges,
      wrongSince' = stampWrong(newMarks, holder, wrongSince, clock),
    }
  }

  /// The reconcile fails (apiserver error/timeout): slot freed, flag
  /// kept — the level-triggered retry contract.
  action reconcileFail = all {
    task != NoTask,
    task' = NoTask,
    dirty' = true,
    holder' = holder, marks' = marks, clock' = clock,
    lastVerify' = lastVerify, strips' = strips,
    holderChanges' = holderChanges,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
  }

  action init = all {
    holder' = NoLeader,
    marks' = PODS.mapBy(p => false),
    // Production starts dirty (the init-true flag: a restarted
    // container may inherit a previous incarnation's marks).
    dirty' = true,
    task' = NoTask,
    clock' = 0,
    lastVerify' = 0,
    strips' = 0,
    holderChanges' = 0,
    wrongSince' = PODS.mapBy(p => -1),
  }

  action holderChangeAny = {
    nondet h = Set(NoLeader, Leader("us"), Leader("other")).oneOf()
    holderChange(h)
  }
  action externalStripAny = {
    nondet p = PODS.oneOf()
    externalStrip(p)
  }

  action step = any {
    tickRound,
    holderChangeAny,
    externalStripAny,
    verify,
    reconcileSpawn,
    reconcileComplete,
    reconcileFail,
  }

  // --- deterministic protocol runs -----------------------------------
  /// The structural close, end to end: acquire, converge, get
  /// externally stripped, and the cadence-forced verify re-discovers
  /// and the next reconcile re-asserts — the divergence is bounded.
  run verifyConvergesRun =
    init
      .then(holderChange(Leader("us")))
      .then(reconcileSpawn)
      .then(reconcileComplete)
      .then(externalStrip("us"))
      .then(tickRound)
      .then(tickRound)
      .then(verify)
      .then(reconcileSpawn)
      .then(reconcileComplete)
      .expect(all {
        marks.get("us"),
        not(dirty),
        wrongSince.get("us") == -1,
      })
}

// =========================================================================
// The wired regime: small cadence, one strip, two holder changes.
module leaderMarksBase {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 10,
    MAX_STRIPS = 1,
    MAX_HOLDER_CHANGES = 2
  ).*
}
