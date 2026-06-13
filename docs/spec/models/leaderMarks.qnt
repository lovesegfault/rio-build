// -*- mode: Bluespec; -*-
//
// The leader-marks reconciliation protocol of rio-lease
// (spawn_patch_leader_marks / sweep_peer_leader_marks /
// spawn_verify_leader_marks): the deletion-cost annotation + leader
// label a leading pod keeps converged on itself, sweeps off stale
// peers, and — since bughunt-wave F1 (merged_bug_138) — periodically
// VERIFIES against its own stored pod state.
//
// Since bughunt2-wave slot 8 (bug_181) the dirty flag is a GENERATION
// PAIR, not a bool: DirtyGen{marked, cleared} with dirty ⇔
// marked > cleared. The reconcile loop snapshots `marked` at spawn and
// the success path clears THROUGH the snapshot (cleared := snap), so a
// dirtying site that fires after the snapshot stays dirty by
// arithmetic — no is_leader-ordering premise, no polarity re-check,
// and any FUTURE dirtying site (the rebound edge of merged_bug_212 is
// the first) is race-proof by construction. The model mirrors that
// encoding directly: `marked`/`cleared` ints, a task that carries its
// spawn snapshot, and a completion SPLIT into the API write
// (reconcilePatched) and the flag clear (reconcileClear) so that
// dirtying sites land BETWEEN the phases — the interleaving the bool
// encoding lost (rio-lease/src/lib.rs pre-fix store(false)).
//
// Spec rules verified here (markers at the nix/quint.nix wiring):
//   - sched.lease.marks-verify (the bounded-cadence verification on
//     EVERY replica: a marks falsifier on a loop-running pod — either
//     polarity — is re-discovered within one verify interval; the
//     selector-membership residual is the platform's, priced in the
//     rule)
//   - sched.lease.deletion-cost+3 (the holder-aware sweep: the peer
//     sweep spares the current Lease holder)
//
// SCOPE, deliberately narrow: two pods, one reconciler ("us" runs the
// modeled machinery; the peer's own loop is abstracted to ONE
// environment action, otherLoopConverge, with a cadence obligation —
// the same binary runs on every pod by construction, so its repair
// bound is symmetric, and bughunt-13 F1 made that abstraction
// STRUCTURAL instead of a comment), an abstract holder (who the Lease
// names), BOTH external falsifier polarities (the strip nobody's edge
// writers see AND the add nobody's edge writers see — a foreign sweep
// racing a re-acquire, kubectl label, any future actor; bughunt-13 F1:
// the alphabet was born one-sided, so `kubectl label pod <standby>
// role=leader` was unrepresentable while the spec claimed ANY
// falsifier bounded), and the single-flight reconcile/verify task.
// Time is the reconciler's round counter.
//
// THE INVARIANTS:
//   - marksDivergenceBounded (cause-tagged): whenever OUR pod's stored
//     marks diverge from our leadership, the divergence is already
//     discovered (dirty, or a task in flight) or it is externally
//     caused (ByStrip OR ByAdd — both polarities since F1) and younger
//     than the verify cadence. Edge-caused and
//     stale-write-caused divergence carries NO age window: the edge
//     writers mark the generation in the same transition, so
//     discovery is immediate — a claim the bool encoding could not
//     make (the clear clobbered the mark).
//   - peerDivergenceBounded: the PEER pod's divergence — any cause,
//     including our own sweep's read-then-patch TOCTOU and the
//     external add — clears within the same cadence bound (its own
//     loop runs the same machinery; the old model carried this as a
//     comment beside the sweep, F1 makes it a checked claim).
//   - nonLeaderDivergenceBounded: THE F1 BOUND, named — a non-leading
//     pod carrying leader marks (the kubectl-label falsifier class
//     the leader-only Service routes on) is discovered or converged
//     within one standby cadence + the task-parking deadline, for
//     EVERY pod. The lease-f1-standby-add calibration twin restores
//     the leader-only verify world and must violate it.
//   - notClobbered: no clear ever erases a mark made after the
//     snapshot the clear was spawned against — the DirtyGen
//     arithmetic itself. The lease-181-bool-clear calibration twin
//     restores the bool semantics (clear-all) and must violate it.
//
// wrongSince/wrongCause DISCIPLINE (review note from the wave specs):
// the stamps are single derived helpers applied in EVERY action's
// frame — there is no enumerated writer list to fall out of sync
// with, which is what keeps the green check non-vacuous.
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
  /// External ADDS per trace (the falsifier's other polarity —
  /// bughunt-13 F1; 0 = disabled). Priced exactly like MAX_STRIPS:
  /// the budget form every environment action carries.
  const MAX_ADDS: int
  /// Holder changes per trace (state-space bound).
  const MAX_HOLDER_CHANGES: int
  /// Rebound edges per trace (the merged_bug_212 dirtying site;
  /// 0 = disabled). A rebound keeps the holder — it is a mark with no
  /// desire change, exactly the "future dirtying site" class the
  /// generation arithmetic must absorb.
  const MAX_REBOUNDS: int
  /// In-flight task residency cap in rounds (production: the
  /// patch_timeout/call_timeout the loop wraps every spawned API call
  /// in — rio-lease/src/lib.rs spawn_patch_leader_marks /
  /// spawn_verify_leader_marks; a task cannot park past its request
  /// deadline, it fails). The split-phase completion makes this
  /// load-bearing: a strip landing between the API write and the
  /// clear is undiscoverable until the slot frees, so the divergence
  /// bound's parking term is exactly this deadline. The old atomic
  /// completion hid the window (the write repaired in-flight strips
  /// by construction) — an artifact, not a production truth.
  const TASK_TIMEOUT: int

  type Pod = str
  /// Who the Lease names. None = vacant.
  type HolderState = | NoLeader | Leader(Pod)
  /// Why a pod's marks diverged (stamped alongside wrongSince by the
  /// same derived helper): an external strip (verify-cadence-bounded),
  /// an external ADD (the F1 polarity — standby-self-check-bounded),
  /// one of our own leadership edges (marks the generation in the same
  /// transition — discovery immediate), or a stale reconcile write
  /// landing after the desire moved (the racing edge already marked).
  type Cause = | NoCause | ByStrip | ByAdd | ByEdge | ByWrite
  /// The single-flight reconcile/verify slot, split into its two
  /// production phases: Reconcile = spawned, API write in flight
  /// (carries the polarity + the same-pass holder read of the
  /// holder-aware sweep — rider R2 of merged_bug_138 — and the
  /// DirtyGen snapshot taken at spawn); Patched = the write landed,
  /// the clear not yet executed. Dirtying sites interleave with BOTH
  /// boundaries.
  type TaskState =
    | NoTask
    | Reconcile({ polarity: bool, sweepHolder: HolderState, snap: int, since: int })
    | Patched({ snap: int, since: int })

  val PODS: Set[Pod] = Set("us", "other")

  // --- state -------------------------------------------------------------
  /// The Lease's holder (abstract; drives desire).
  var holder: HolderState
  /// Stored leader marks per pod (true = leader marks present).
  var marks: Pod -> bool
  /// DirtyGen.marked: total dirtying events so far (monotone).
  var marked: int
  /// DirtyGen.cleared: the highest spawn snapshot a successful clear
  /// has consumed (cleared <= marked always; dirty ⇔ marked > cleared).
  var cleared: int
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
  /// Per-pod divergence cause, stamped by the paired helper
  /// (see stampCause); NoCause when converged.
  var wrongCause: Pod -> Cause
  /// External strips so far.
  var strips: int
  /// External adds so far (the F1 polarity).
  var adds: int
  /// Holder changes so far.
  var holderChanges: int
  /// Rebound edges so far.
  var rebounds: int
  /// Latch with a live writer (reconcileClear computes it from the
  /// transition's own values): a clear erased a mark made after the
  /// snapshot it was spawned against. Identically false under the
  /// clear-through semantics; the bool twin's clear-all writes it
  /// true.
  var clobberErased: bool

  // --- desire / divergence ------------------------------------------------
  def leadingOf(p: Pod, h: HolderState): bool = h == Leader(p)
  /// Desired marks = leadership (cost 1 + label iff leading).
  def wrongUnder(m: Pod -> bool, h: HolderState, p: Pod): bool =
    m.get(p) != leadingOf(p, h)
  /// DirtyGen.dirty: the derived flag (production is_dirty()).
  def dirty: bool = marked > cleared

  /// THE single divergence stamp, applied in every action's frame:
  /// newly-wrong stamps now, still-wrong keeps its stamp, converged
  /// clears. No enumerated writer list.
  def stampWrong(m: Pod -> bool, h: HolderState, ws: Pod -> int, now: int): Pod -> int =
    PODS.mapBy(p =>
      if (wrongUnder(m, h, p)) (if (ws.get(p) == -1) now else ws.get(p))
      else -1)

  /// The paired cause stamp (same discipline, same frame sites):
  /// newly-wrong takes the ACTING transition's cause, still-wrong
  /// keeps its cause, converged clears. `ws` is the PREVIOUS
  /// wrongSince map — "newly wrong" is its -1.
  def stampCause(m: Pod -> bool, h: HolderState, ws: Pod -> int, wc: Pod -> Cause, c: Cause): Pod -> Cause =
    PODS.mapBy(p =>
      if (wrongUnder(m, h, p)) (if (ws.get(p) == -1) c else wc.get(p))
      else NoCause)

  // --- invariants / witnesses ---------------------------------------------
  /// Bounds tripwire.
  val boundsOK = and {
    clock <= MAX_TIME,
    strips <= MAX_STRIPS,
    adds <= MAX_ADDS,
    holderChanges <= MAX_HOLDER_CHANGES,
    rebounds <= MAX_REBOUNDS,
    cleared <= marked,
  }

  /// The headline, cause-tagged: OUR pod's divergence is discovered
  /// (dirty / task in flight), or it is EXTERNALLY caused — ByStrip
  /// or, since F1, ByAdd: the verify pass runs at both leadership
  /// polarities, so a foreign add on the standby is discovered by the
  /// same cadence that discovers a foreign strip on the leader — and
  /// younger than the verify cadence PLUS the task-parking deadline.
  /// The bound's two terms are the verify interval (a clean-idle loop
  /// must verify within VERIFY_EVERY) and the API-call deadline (a
  /// falsification landing inside the Patched window is
  /// undiscoverable until the slot frees, at most TASK_TIMEOUT
  /// rounds; the split-phase model surfaces this term, the old atomic
  /// completion hid it). Edge- and write-caused divergence gets NO age
  /// window — the edge writers mark() in the same transition, so the
  /// dirty disjunct must already hold; the bool encoding could not
  /// sustain this (the clear clobbered the mark, the
  /// lease-181-bool-clear twin's violation). The repair itself is the
  /// next reconcile (level-triggered; pinned by the production loop
  /// tests) — the model pins DISCOVERY boundedness, which is what the
  /// pre-fix machinery lacked.
  val marksDivergenceBounded =
    wrongUnder(marks, holder, "us") implies or {
      dirty,
      task != NoTask,
      and {
        or {
          wrongCause.get("us") == ByStrip,
          wrongCause.get("us") == ByAdd,
        },
        clock - wrongSince.get("us") <= VERIFY_EVERY + TASK_TIMEOUT,
      },
    }

  /// The PEER pod's divergence — any cause: our sweep's
  /// read-then-patch TOCTOU (ByWrite), an edge its observation lags
  /// (ByEdge), either external polarity — clears within the same
  /// cadence bound. The repair machinery is the peer's OWN loop
  /// (otherLoopConverge + the tickRound cadence obligation): the same
  /// binary runs there by construction, which is exactly the symmetry
  /// argument the pre-F1 model carried as prose beside the sweep
  /// comment; this val is that argument, checked.
  val peerDivergenceBounded =
    wrongUnder(marks, holder, "other") implies
      clock - wrongSince.get("other") <= VERIFY_EVERY + TASK_TIMEOUT

  /// THE F1 BOUND, named: a NON-leading pod carrying leader marks —
  /// the falsifier class the leader-only Service routes on (`kubectl
  /// label pod <standby> …role=leader`; production: an un-retryable
  /// Trailers-Only UNAVAILABLE data path until repaired) — is
  /// discovered (our own dirty/task, for the modeled pod) or younger
  /// than one standby cadence + the task-parking deadline, for EVERY
  /// pod. Unsatisfiable in the pre-F1 world: no action could even
  /// construct the state (no add in the alphabet), and had it
  /// existed, nothing would have repaired it — the
  /// lease-f1-standby-add twin pins both halves by restoring the
  /// leader-only verify and expecting this val to fall.
  val nonLeaderDivergenceBounded = PODS.forall(p =>
    (not(leadingOf(p, holder)) and marks.get(p)) implies or {
      p == "us" and (dirty or task != NoTask),
      clock - wrongSince.get(p) <= VERIFY_EVERY + TASK_TIMEOUT,
    })

  /// The DirtyGen arithmetic itself: no clear ever erases a
  /// post-snapshot mark. Written live by reconcileClear from its own
  /// transition values (never a ghost assignment); the
  /// lease-181-bool-clear twin restores clear-all and must violate.
  val notClobbered = not(clobberErased)

  // The holder-aware sweep half (deletion-cost+3) is encoded
  // structurally in reconcilePatched (the sweep skips sweepHolder,
  // captured in the same pass) and pinned at the production level by
  // the red-first unit peer_sweep_spares_current_lease_holder — NOT by
  // a model invariant: the spawn-to-complete TOCTOU (a holder change
  // between the capture and the strip) makes "the holder is never
  // swept" deliberately false, and that residual is exactly what the
  // verify cadence bounds (the victim's own loop re-discovers within
  // the peerDivergenceBounded window — since F1 a CHECKED claim, not
  // an argued one). A vacuous "holds by construction" val here would
  // be decoration. The sweep's STRIP face, dead in every reachable
  // pre-F1 state (nothing could set marks["other"]; bughunt-13 F2),
  // is live since externalAdd joined the alphabet: the
  // sweepStripsForeignPeerLabelRun drives it deterministically and
  // the noPeerMarks witness proves the explored space reaches
  // other-marked states at all. The spare conjunct
  // (Leader(p) != taskSweepHolder) remains production-pinned: with
  // the spawn-time capture and two pods, a true-polarity task always
  // captures Leader("us"), so the spare can never bind in-model —
  // the capture is deliberately EARLIER than production's in-task
  // holder read (a conservatively wider TOCTOU), and the unit pin
  // owns the spare's truth.

  /// Witness (expect-violation): the peer-marked state is actually
  /// reached in the explored space — the sweep branch the
  /// deletion-cost+3 marker rides stopped being dead text when
  /// externalAdd joined the alphabet (bughunt-13 F2; the bug_083
  /// anti-vacuity pattern).
  val noPeerMarks = not(marks.get("other"))

  /// Witness (expect-violation): the external strip actually fires —
  /// the green marksDivergenceBounded verdict is about a space where
  /// the falsifier bites.
  val noStrip = strips == 0

  /// Witness (expect-violation): the external ADD actually fires —
  /// the green nonLeaderDivergenceBounded verdict is about a space
  /// where the F1 falsifier polarity bites (the bug_083 anti-vacuity
  /// pattern, same as noStrip).
  val noAdd = adds == 0

  /// Witness (expect-violation): the rebound dirtying site actually
  /// fires — the notClobbered verdict covers the post-212 writer set,
  /// not just the original edges.
  val noRebound = rebounds == 0

  // --- actions --------------------------------------------------------
  /// One reconciler round passes. The verify-cadence cap (the
  /// loopInterval pattern of leaderElection.qnt): a clean, idle
  /// reconciler — leader OR standby, since F1 — cannot advance past
  /// the cadence without the verify pass having run; the production
  /// loop reaches the verify gate every round at both polarities. The
  /// peer-loop cap is the symmetric obligation: the peer pod runs the
  /// same binary, so its divergence cannot outlive the same cadence
  /// bound without its own loop (otherLoopConverge) having repaired
  /// it — the obligation form of the symmetry argument the pre-F1
  /// model left as prose.
  // Mirrors: rio-lease/src/lib.rs:run_lease_loop_with_client
  action tickRound = all {
    clock < MAX_TIME,
    (not(dirty) and task == NoTask) implies
      (clock + 1 - lastVerify <= VERIFY_EVERY),
    // The peer pod's own cadence obligation (its loop is environment;
    // the same machinery runs there by construction).
    wrongUnder(marks, holder, "other") implies
      (clock + 1 - wrongSince.get("other") <= VERIFY_EVERY + TASK_TIMEOUT),
    // The task-residency cap (production patch_timeout/call_timeout):
    // an in-flight API call cannot park past its request deadline —
    // the round after TASK_TIMEOUT it has either completed or failed.
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The Lease changes hands (steal, graceful release + steal, the
  /// environment's election). OUR edge writers fire: any change of
  /// our own leadership MARKS the generation (the production
  /// acquire/lose edges — six store(true) sites, all mark() since
  /// bug_181).
  action holderChange(h: HolderState): bool = all {
    holderChanges < MAX_HOLDER_CHANGES,
    h != holder,
    holder' = h,
    marked' = if (leadingOf("us", h) != leadingOf("us", holder)) marked + 1 else marked,
    cleared' = cleared,
    marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges + 1, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, h, wrongSince, clock),
    wrongCause' = stampCause(marks, h, wrongSince, wrongCause, ByEdge),
  }

  /// The rebound edge (merged_bug_212): the holder is unchanged — we
  /// lost and re-acquired inside one observation gap — and the
  /// on_rebound hook marks. No desire change, no divergence created;
  /// PURELY a post-snapshot mark, the writer class the generation
  /// arithmetic exists to absorb (and the clobber the bool twin
  /// erases).
  // Mirrors: rio-lease/src/lib.rs:observe_held_while_believing
  action reboundRedirty = all {
    rebounds < MAX_REBOUNDS,
    leadingOf("us", holder),
    marked' = marked + 1,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds + 1,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// THE FALSIFIER, strip polarity: an external actor strips a pod's
  /// marks — a foreign sweep racing a re-acquire (the read-then-patch
  /// TOCTOU), kubectl, any future actor. NO local generation moves:
  /// the whole point is that the edge-writer set does not see it.
  action externalStrip(p: Pod): bool = all {
    strips < MAX_STRIPS,
    marks.get(p),
    marks' = marks.put(p, false),
    strips' = strips + 1,
    holder' = holder, marked' = marked, cleared' = cleared,
    task' = task, clock' = clock,
    lastVerify' = lastVerify, holderChanges' = holderChanges,
    adds' = adds,
    rebounds' = rebounds, clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks.put(p, false), holder, wrongSince, clock),
    wrongCause' = stampCause(marks.put(p, false), holder, wrongSince, wrongCause, ByStrip),
  }

  /// THE FALSIFIER, add polarity (bughunt-13 F1): an external actor
  /// ADDS leader marks to a pod — `kubectl label pod <standby>
  /// …role=leader`, a foreign controller, any future actor. NO local
  /// generation moves, no edge fires, the leader's own-pod verify
  /// never sees this pod, and the peer sweep is dormant while the
  /// holder's loop is clean ("Steady state spawns nothing.") — which
  /// is exactly why the pre-F1 model's one-sided alphabet kept
  /// marksDivergenceBounded green over a world where this state was
  /// silently unbounded in production. An add on a pod already
  /// carrying marks is a no-op and does not burn the budget.
  action externalAdd(p: Pod): bool = all {
    adds < MAX_ADDS,
    not(marks.get(p)),
    marks' = marks.put(p, true),
    adds' = adds + 1,
    holder' = holder, marked' = marked, cleared' = cleared,
    task' = task, clock' = clock,
    lastVerify' = lastVerify, holderChanges' = holderChanges,
    strips' = strips,
    rebounds' = rebounds, clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks.put(p, true), holder, wrongSince, clock),
    wrongCause' = stampCause(marks.put(p, true), holder, wrongSince, wrongCause, ByAdd),
  }

  /// The PEER pod's own loop converging its own marks — the same
  /// binary by construction: its edges, its lose-edge reconcile, its
  /// own verify at both polarities (since F1), compressed to one
  /// step. This reconciler's plane models "us"; the peer's plane is
  /// environment with the tickRound cadence obligation as its
  /// fairness — exactly the per-pod symmetry the production fleet
  /// has, made structural instead of a comment.
  // Environment: the peer pod's own lease loop (the identical binary, this plane's "us" machinery from its side) compressed to one converge step; fairness = the tickRound peer cadence cap
  action otherLoopConverge = all {
    wrongUnder(marks, holder, "other"),
    marks' = marks.put("other", leadingOf("other", holder)),
    holder' = holder, marked' = marked, cleared' = cleared,
    task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks.put("other", leadingOf("other", holder)), holder, wrongSince, clock),
    wrongCause' = stampCause(marks.put("other", leadingOf("other", holder)), holder, wrongSince, wrongCause, NoCause),
  }

  /// The bounded-cadence verify pass (sched.lease.marks-verify): a
  /// clean, idle reconciler — AT EITHER LEADERSHIP POLARITY, since F1
  /// — reads its OWN pod and MARKS on divergence (store(true) became
  /// mark() — bug_181). The standby polarity is the F1 detector: a
  /// foreign add on a non-leading pod is a divergence at
  /// leading=false, and the re-dirtied reconcile strips it. Enabled
  /// any round (the cadence cap on tickRound FORCES it within
  /// VERIFY_EVERY; firing earlier is sound).
  // Mirrors: rio-lease/src/lib.rs:spawn_verify_leader_marks
  action verify = all {
    not(dirty),
    task == NoTask,
    lastVerify' = clock,
    marked' = if (wrongUnder(marks, holder, "us")) marked + 1 else marked,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    strips' = strips, adds' = adds, holderChanges' = holderChanges,
    rebounds' = rebounds, clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The reconcile spawns: dirty + free slot. Captures the polarity,
  /// the holder (the same-pass Lease read of the holder-aware sweep —
  /// rider R2), and the DirtyGen SNAPSHOT the eventual clear will
  /// clear through (production: loop reads marked at spawn).
  // Mirrors: rio-lease/src/lib.rs:maybe_spawn_leader_marks
  action reconcileSpawn = all {
    dirty,
    task == NoTask,
    task' = Reconcile({ polarity: leadingOf("us", holder), sweepHolder: holder, snap: marked, since: clock }),
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// Field accessors for the in-flight task (-defaults for the wrong
  /// phase; every consumer guards the phase first — the leaderElection
  /// accessor convention).
  def taskPolarity: bool = match task {
    | NoTask => false
    | Reconcile(r) => r.polarity
    | Patched(_) => false
  }
  def taskSweepHolder: HolderState = match task {
    | NoTask => NoLeader
    | Reconcile(r) => r.sweepHolder
    | Patched(_) => NoLeader
  }
  def taskSnap: int = match task {
    | NoTask => -1
    | Reconcile(r) => r.snap
    | Patched(p) => p.snap
  }
  def taskSince: int = match task {
    | NoTask => -1
    | Reconcile(r) => r.since
    | Patched(p) => p.since
  }

  /// Phase 1 of the completion: the API write lands — own marks
  /// written at the captured polarity; while leading, the sweep strips
  /// every OTHER labeled pod EXCEPT the captured holder
  /// (sched.lease.deletion-cost+3 — the just-re-acquired peer is
  /// spared). The flag is NOT touched here: the clear is a separate
  /// transition (production: the task returns, THEN the loop clears),
  /// which is exactly where the bool encoding lost its race.
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action reconcilePatched = {
    val newMarks = PODS.mapBy(p =>
      if (p == "us") taskPolarity
      else if (taskPolarity and marks.get(p) and Leader(p) != taskSweepHolder) false
      else marks.get(p))
    all {
      match task { | Reconcile(_) => true | _ => false },
      marks' = newMarks,
      task' = Patched({ snap: taskSnap, since: taskSince }),
      holder' = holder, marked' = marked, cleared' = cleared,
      clock' = clock, lastVerify' = lastVerify,
      strips' = strips, adds' = adds, holderChanges' = holderChanges,
      rebounds' = rebounds, clobberErased' = clobberErased,
      wrongSince' = stampWrong(newMarks, holder, wrongSince, clock),
      wrongCause' = stampCause(newMarks, holder, wrongSince, wrongCause, ByWrite),
    }
  }

  /// Phase 2: the success path clears THROUGH the spawn snapshot
  /// (production clear_through(snap): cleared := max(cleared, snap)).
  /// A mark that landed after the snapshot keeps marked > cleared —
  /// the loop stays dirty and re-reconciles; nothing is re-derived,
  /// nothing is re-checked. clobberErased is written LIVE from this
  /// transition's own values: did this clear erase a post-snapshot
  /// mark? (clean afterwards AND marks existed past the snapshot).
  /// Identically false here; the bool twin's clear-all makes it fire.
  // Mirrors: rio-lease/src/lib.rs:clear_through
  action reconcileClear = {
    val newCleared = if (taskSnap > cleared) taskSnap else cleared
    all {
      match task { | Patched(_) => true | _ => false },
      cleared' = newCleared,
      clobberErased' = (clobberErased or
        (newCleared >= marked and marked > taskSnap)),
      task' = NoTask,
      holder' = holder, marks' = marks, marked' = marked,
      clock' = clock, lastVerify' = lastVerify,
      strips' = strips, adds' = adds, holderChanges' = holderChanges,
      rebounds' = rebounds,
      wrongSince' = stampWrong(marks, holder, wrongSince, clock),
      wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
    }
  }

  /// The reconcile fails (apiserver error/timeout, EITHER phase): slot
  /// freed, generations untouched — marked > cleared still holds, the
  /// level-triggered retry contract with no store(true) re-arm needed.
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action reconcileFail = all {
    task != NoTask,
    task' = NoTask,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  action init = all {
    holder' = NoLeader,
    marks' = PODS.mapBy(p => false),
    // Production starts dirty (DirtyGen::new_dirty(): a restarted
    // container may inherit a previous incarnation's marks).
    marked' = 1,
    cleared' = 0,
    task' = NoTask,
    clock' = 0,
    lastVerify' = 0,
    strips' = 0,
    adds' = 0,
    holderChanges' = 0,
    rebounds' = 0,
    clobberErased' = false,
    wrongSince' = PODS.mapBy(p => -1),
    wrongCause' = PODS.mapBy(p => NoCause),
  }

  /// The Lease changes hands (steal, graceful release + steal, the
  /// environment's election); see holderChange.
  // Environment: the Lease moving at the apiserver is the election plane's move (leaderElection.qnt owns it); the fused same-transition edge-mark abstracts the production acquire/lose arms' mark() in run_lease_loop_with_client
  action holderChangeAny = {
    nondet h = Set(NoLeader, Leader("us"), Leader("other")).oneOf()
    holderChange(h)
  }
  // Environment: the strip falsifier — kubectl label removal / a foreign sweep racing a re-acquire; budgeted MAX_STRIPS, and no local generation moves by design (the edge-writer set does not see it)
  action externalStripAny = {
    nondet p = PODS.oneOf()
    externalStrip(p)
  }
  // Environment: the add falsifier (F1) — kubectl label / a foreign controller stamping leader marks; budgeted MAX_ADDS, and no local generation moves by design (the edge-writer set does not see it)
  action externalAddAny = {
    nondet p = PODS.oneOf()
    externalAdd(p)
  }

  action step = any {
    tickRound,
    holderChangeAny,
    reboundRedirty,
    externalStripAny,
    externalAddAny,
    otherLoopConverge,
    verify,
    reconcileSpawn,
    reconcilePatched,
    reconcileClear,
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
      .then(reconcilePatched)
      .then(reconcileClear)
      .then(externalStrip("us"))
      .then(tickRound)
      .then(tickRound)
      .then(verify)
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reconcileClear)
      .expect(all {
        marks.get("us"),
        not(dirty),
        wrongSince.get("us") == -1,
      })

  /// The F1 structural close, end to end: converge as STANDBY (the
  /// peer holds the Lease), get externally LABELED leader (`kubectl
  /// label pod <standby> …role=leader` — no edge, no generation move,
  /// the leader's verify blind to this pod, the peer sweep dormant),
  /// and the standby's own cadence-forced verify re-discovers and the
  /// next reconcile STRIPS — the divergence is bounded at the
  /// polarity the pre-F1 alphabet could not even represent.
  run standbyStripConvergesRun =
    init
      .then(holderChange(Leader("other")))
      .then(otherLoopConverge)
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reconcileClear)
      .then(externalAdd("us"))
      .then(tickRound)
      .then(tickRound)
      .then(verify)
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reconcileClear)
      .expect(all {
        not(marks.get("us")),
        marks.get("other"),
        not(dirty),
        wrongSince.get("us") == -1,
      })

  /// The sweep's strip face, end to end (bughunt-13 F2 — the branch
  /// was dead text until externalAdd joined the alphabet): WE lead, a
  /// foreign actor labels the PEER, and OUR reconcile's sweep strips
  /// it (the peer's own loop never fires in this trace — the strip is
  /// the sweep's, not otherLoopConverge's). The holder-aware spare
  /// conjunct stays production-pinned (see the sweep comment above).
  run sweepStripsForeignPeerLabelRun =
    init
      .then(holderChange(Leader("us")))
      .then(externalAdd("other"))
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reconcileClear)
      .expect(all {
        marks.get("us"),
        not(marks.get("other")),
        not(dirty),
        wrongSince.get("other") == -1,
      })

  /// The DirtyGen save, end to end: a rebound marks BETWEEN the API
  /// write and the clear, and the clear-through arithmetic keeps the
  /// loop dirty — the post-snapshot mark survives (the bool encoding
  /// erased it; lease-181-bool-clear pins that world).
  run clearKeepsPostSnapMarkRun =
    init
      .then(holderChange(Leader("us")))
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reboundRedirty)
      .then(reconcileClear)
      .expect(all {
        dirty,
        not(clobberErased),
        marks.get("us"),
      })
}

// =========================================================================
// The wired regime: small cadence, one strip, ONE ADD (the F1
// polarity), two holder changes, one rebound.
module leaderMarksBase {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 10,
    MAX_STRIPS = 1,
    MAX_ADDS = 1,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 1,
    TASK_TIMEOUT = 4
  ).*
}
