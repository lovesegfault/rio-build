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
//   - sched.lease.deletion-cost+4 (the holder-aware sweep: the peer
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
  /// Failed verify attempts per trace (bughunt-13 F3; 0 = disabled).
  /// Production's verify GET fails (apiserver error/timeout) and the
  /// loop skips to the next cadence point having learned NOTHING —
  /// the pre-F3 model's verify was infallible, so the wired two-term
  /// divergence bound silently assumed every attempt succeeds; one
  /// real failure exceeded it. Budgeted like every other environment
  /// action; the divergence bounds carry the failure term.
  const MAX_VERIFY_FAILS: int
  /// Failed RECONCILE attempts per trace (bughunt-13 F6). Pre-F6,
  /// reconcileFail was the ONLY unbudgeted environment action — the
  /// spawn→fail cycle satisfied the divergence bounds' dirty disjunct
  /// FOREVER (discovery-not-repair: a livelock that repairs nothing
  /// is "discovered" at every state), and no convergence property
  /// existed anywhere. Production's analog is the persistent PATCH
  /// 403: a platform failure the code prices as a warn-stream
  /// forever-state. Budgeting the failures is what makes a
  /// convergence claim STATEABLE: under at most this many failures,
  /// divergence must actually CLEAR (marksConvergenceBounded in the
  /// convergence regime) — beyond the budget is the platform's named
  /// failure, never a silent satisfaction of the repair contract.
  const MAX_RECONCILE_FAILS: int
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
  /// Failed verify attempts so far (the F3 fallibility).
  var verifyFails: int
  /// Failed reconcile attempts so far (the F6 budget).
  var reconcileFails: int
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

  /// The discovery bound under FALLIBLE verification (bughunt-13 F3):
  /// the first cadence term covers the next attempt; each budgeted
  /// failure (a GET error/timeout that consumes the cadence point and
  /// learns nothing) defers one full cadence; the parking term covers
  /// a falsification landing inside the Patched window. Pre-F3 the
  /// bounds used the bare two-term form — one real failure exceeded
  /// it (divergenceWithinTwoTermBound, the wired twin, pins exactly
  /// that).
  def discoveryBound: int = (1 + MAX_VERIFY_FAILS) * VERIFY_EVERY + TASK_TIMEOUT

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
    verifyFails <= MAX_VERIFY_FAILS,
    reconcileFails <= MAX_RECONCILE_FAILS,
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
  /// window — UNDER THIS MODULE'S FUSED ABSTRACTION: holderChange
  /// fires the edge mark() in the same transition the Lease moves, so
  /// the dirty disjunct must already hold. That is a premise, not a
  /// production fact — every production edge mark is
  /// OBSERVATION-time, a round-trip behind the apiserver — and the
  /// leaderMarksObservationLag regime (bughunt-13 F5) unfuses the two
  /// and prices the real window (REPAIR_BOUND: own lagged observation
  /// or the new holder's sweep). The bool encoding could not
  /// sustain even the fused claim (the clear clobbered the mark, the
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
        clock - wrongSince.get("us") <= discoveryBound,
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
      clock - wrongSince.get("other") <= discoveryBound

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
      clock - wrongSince.get(p) <= discoveryBound,
    })

  /// The DirtyGen arithmetic itself: no clear ever erases a
  /// post-snapshot mark. Written live by reconcileClear from its own
  /// transition values (never a ghost assignment); the
  /// lease-181-bool-clear twin restores clear-all and must violate.
  val notClobbered = not(clobberErased)

  // The holder-aware sweep half (sched.lease.deletion-cost) is encoded
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
  /// deletion-cost marker rides stopped being dead text when
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

  /// Witness (expect-violation): the verify failure actually fires —
  /// the widened discoveryBound verdicts are about a space where the
  /// apiserver actually fails attempts (bughunt-13 F3).
  val noVerifyFail = verifyFails == 0

  /// THE PRE-F3 TWO-TERM BOUND, kept as the wired falsify twin
  /// (expect-violation in the base regime): with verify fallible, a
  /// SINGLE failed attempt defers discovery one full cadence past
  /// this form — the audit's exact finding ("one failure exceeds the
  /// wired 2-term bound"). If this ever stops violating, either the
  /// failure letter went dead (noVerifyFail catches that
  /// independently) or the bound derivation regressed.
  val divergenceWithinTwoTermBound =
    wrongUnder(marks, holder, "us") implies or {
      dirty,
      task != NoTask,
      clock - wrongSince.get("us") <= VERIFY_EVERY + TASK_TIMEOUT,
    }

  /// THE CONVERGENCE BOUND (bughunt-13 F6): how long OUR pod's marks
  /// may stay continuously wrong before they actually CONVERGE — not
  /// merely "are discovered". Terms: discovery (discoveryBound), then
  /// per repair attempt up to TASK_TIMEOUT of slot residency with the
  /// spawn obligation re-arming the next attempt the round the slot
  /// frees; failed attempts are budgeted (MAX_RECONCILE_FAILS), and a
  /// mid-repair desire flip (each budgeted holder change) restarts
  /// the chain at the new polarity while the continuous-wrong stamp
  /// keeps the ORIGINAL divergence time — so each holder change buys
  /// one more discovery+repair chain against the same stamp.
  def convergenceBound: int =
    (1 + MAX_HOLDER_CHANGES) * (discoveryBound + (1 + MAX_RECONCILE_FAILS) * TASK_TIMEOUT)

  /// THE F6 KILL: repair is a CONVERGENCE property, not an enabled
  /// action. Pre-F6 the model's only repair claim was
  /// marksDivergenceBounded's dirty disjunct — satisfied FOREVER by a
  /// spawn→fail livelock that repairs nothing (discovery-not-repair;
  /// the audit's class-e shape; production analog: the persistent
  /// PATCH 403 forever-state). Under budgeted failures this val
  /// demands the divergence actually CLEAR. Wired in the convergence
  /// regime (whose horizon exceeds the bound — in the base regime
  /// MAX_TIME < convergenceBound makes it horizon-vacuous, recorded
  /// at the wiring); the lease-f6-fail-livelock twin un-budgets the
  /// failures and must violate it while the OLD disjunct form stays
  /// green over the same livelock — the kill, recorded verbatim in
  /// the introducing commit.
  val marksConvergenceBounded =
    wrongUnder(marks, holder, "us") implies
      clock - wrongSince.get("us") <= convergenceBound

  /// Witness (expect-violation): the rebound dirtying site actually
  /// fires — the notClobbered verdict covers the post-212 writer set,
  /// not just the original edges.
  val noRebound = rebounds == 0

  // --- actions --------------------------------------------------------
  /// One reconciler round passes. The verify-cadence cap is
  /// MULTIPLE-KEYED with skip-on-busy since bughunt-13 F4 — the
  /// production schedule, adopted: attempts fire at rounds where
  /// renew_rounds_started % MARKS_VERIFY_EVERY == 0 iff the marks are
  /// clean and the single-flight slot is free; a busy multiple is
  /// SKIPPED whole (the next attempt waits a full interval). The cap
  /// here forces exactly that: a clean, idle reconciler — leader OR
  /// standby, since F1 — cannot tick OUT of an unserviced multiple
  /// round; every other round ticks freely. The skip term costs the
  /// DIVERGENCE bound nothing in-model because busy implies dirty
  /// (reconcile tasks spawn only from dirt and the clear frees the
  /// slot in the same transition), so a skipped multiple is always a
  /// multiple whose divergence was already discovered — and the
  /// production-side bridge for that pricing is the R34 compile
  /// assert in rio-lease (a marks task's deadline + one round fits
  /// inside one verify interval, so a verify task can never park
  /// ACROSS a multiple and manufacture the clean+busy skip the model
  /// excludes). Pre-F4 the model forced deadline-from-lastVerify — a
  /// STRONGER schedule production never ran, papered over by a "runs
  /// every VERIFY_EVERY rounds by construction" comment that was
  /// false (skip-on-busy defers a full interval).
  /// The peer-loop cap is the symmetric obligation: the peer pod runs
  /// the same binary, so its divergence cannot outlive the same
  /// cadence bound without its own loop (otherLoopConverge) having
  /// repaired it — the obligation form of the symmetry argument the
  /// pre-F1 model left as prose.
  // Mirrors: rio-lease/src/lib.rs:run_lease_loop_with_client
  action tickRound = all {
    clock < MAX_TIME,
    (not(dirty) and task == NoTask and clock % VERIFY_EVERY == 0) implies
      lastVerify == clock,
    // The spawn obligation (bughunt-13 F6): a dirty loop with a free
    // slot SPAWNS THIS TICK — production's maybe_spawn_leader_marks
    // runs unconditionally every iteration, so dirt never dwells
    // across a tick boundary idly. Without this, the repair side had
    // no forcing at all: discovery was forced (the verify cadence)
    // while the reconcile it owes could stutter forever — exactly
    // the discovery-not-repair face.
    not(dirty and task == NoTask),
    // The peer pod's own cadence obligation (its loop is environment;
    // the same machinery runs there by construction).
    wrongUnder(marks, holder, "other") implies
      (clock + 1 - wrongSince.get("other") <= discoveryBound),
    // The task-residency cap (production patch_timeout/call_timeout):
    // an in-flight API call cannot park past its request deadline —
    // the round after TASK_TIMEOUT it has either completed or failed.
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
    adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
  /// leading=false, and the re-dirtied reconcile strips it.
  /// MULTIPLE-KEYED since F4 (production's
  /// is_multiple_of(renew_rounds_started) gate): attempts fire at
  /// cadence multiples only, once per multiple — the tickRound cap
  /// forces the attempt before a clean, idle loop can leave the
  /// multiple round.
  // Mirrors: rio-lease/src/lib.rs:spawn_verify_leader_marks
  action verify = all {
    not(dirty),
    task == NoTask,
    clock % VERIFY_EVERY == 0,
    lastVerify != clock,
    lastVerify' = clock,
    marked' = if (wrongUnder(marks, holder, "us")) marked + 1 else marked,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails, holderChanges' = holderChanges,
    rebounds' = rebounds, clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The verify attempt FAILS (bughunt-13 F3): the GET errors or
  /// times out — the cadence point is consumed (production skips to
  /// the next multiple; lastVerify moves exactly as for a successful
  /// attempt) but NOTHING is learned: no comparison, no mark, the
  /// divergence stays undiscovered for one more cadence. Budgeted
  /// like every other environment action; the pre-F3 model omitted
  /// this letter entirely, which is how the wired two-term bound
  /// silently assumed an infallible apiserver. Production's lane is
  /// loud (warn-after-K-consecutive + the
  /// rio_lease_marks_verify_failures_total counter) — the failure IS
  /// evidence.
  // Environment: the apiserver failing the verify GET (error/timeout) — spawn_verify_leader_marks err arms; budgeted MAX_VERIFY_FAILS; the cadence point is consumed with nothing learned
  action verifyFail = all {
    verifyFails < MAX_VERIFY_FAILS,
    not(dirty),
    task == NoTask,
    clock % VERIFY_EVERY == 0,
    lastVerify != clock,
    lastVerify' = clock,
    verifyFails' = verifyFails + 1,
    reconcileFails' = reconcileFails,
    marked' = marked,
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
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails,
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
  /// (sched.lease.deletion-cost+4 — the just-re-acquired peer is
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
      strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails, holderChanges' = holderChanges,
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
      strips' = strips, adds' = adds, verifyFails' = verifyFails, reconcileFails' = reconcileFails, holderChanges' = holderChanges,
      rebounds' = rebounds,
      wrongSince' = stampWrong(marks, holder, wrongSince, clock),
      wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
    }
  }

  /// The reconcile fails (apiserver error/timeout, EITHER phase): slot
  /// freed, generations untouched — marked > cleared still holds, the
  /// level-triggered retry contract with no store(true) re-arm needed.
  /// BUDGETED since F6 (it was the only unbudgeted environment
  /// action): within budget, the convergence regime's repair claim
  /// must survive every failure; past it is the platform's named
  /// forever-failure (production: the persistent PATCH 403's warn
  /// stream), not a state this model silently absorbs. The failure
  /// CONSUMES at least one round (`clock > taskSince`): production's
  /// retry cycle has a one-round floor regardless of how fast the
  /// PATCH bounced, because the spawn gate runs once per tick — a
  /// fail inside tick N respawns at N+1 earliest. The model prices
  /// that floor at the fail instead of the respawn (same cycle
  /// arithmetic, no extra state); a zero-cost fail would let the
  /// checker burn the whole failure budget inside one instant, a
  /// schedule no production loop can run.
  // Environment: the apiserver failing the marks PATCH/sweep (error/timeout) — spawn_patch_leader_marks err/timeout arms; budgeted MAX_RECONCILE_FAILS; the slot frees and the retry rides the surviving dirt
  action reconcileFail = all {
    reconcileFails < MAX_RECONCILE_FAILS,
    task != NoTask,
    clock > taskSince,
    task' = NoTask,
    reconcileFails' = reconcileFails + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds, verifyFails' = verifyFails,
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
    verifyFails' = 0,
    reconcileFails' = 0,
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
    verifyFail,
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

  /// THE SKIP-ON-BUSY SCHEDULE (bughunt-13 F4, W13-L): the multiple
  /// at clock 4 passes with the single-flight slot BUSY (a reconcile
  /// in flight) — no attempt fires there (production: the
  /// is_multiple_of gate finds the slot taken and skips); the next
  /// attempt waits the FULL interval and lands at clock 8
  /// (lastVerify jumps 0 → 8). The skipped multiple costs the
  /// divergence bound nothing: a busy multiple is dirty by
  /// construction (only dirt spawns reconciles; the clear frees the
  /// slot in the same transition), so its divergence was already
  /// discovered — the in-model pricing whose production-side bridge
  /// is the R34 compile assert in rio-lease (a marks task can never
  /// park ACROSS a multiple).
  run skipOnBusyMultipleRun =
    init
      .then(holderChange(Leader("us")))
      .then(reconcileSpawn)
      .then(reconcilePatched)
      .then(reconcileClear)
      .then(tickRound)
      .then(tickRound)
      .then(tickRound)
      .then(reboundRedirty)
      .then(reconcileSpawn)
      .then(tickRound)
      .then(reconcilePatched)
      .then(tickRound)
      .then(reconcileClear)
      .then(tickRound)
      .then(tickRound)
      .then(tickRound)
      .then(verify)
      .expect(all {
        lastVerify == 8,
        clock == 8,
        not(dirty),
        marks.get("us"),
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
// polarity), ONE verify failure (the F3 fallibility — the widened
// discoveryBound is exercised, and the pre-F3 two-term bound twin
// violates here), two holder changes, one rebound. The cadence/parking
// proportions are PRODUCTION-SHAPED since F3: TASK_TIMEOUT <
// VERIFY_EVERY (a verify call deadline is ~1 renew round, far under
// the 12-round cadence) — the pre-F3 constants had the parking term
// LARGER than the cadence, which let the two-term bound absorb a
// failure by sheer slack and would have made the F3 twin
// unfalsifiable (the "bound survives only by unpinned constant
// margins" face of the audit's F4, dying here a commit early).
module leaderMarksBase {
  import leaderMarks(
    VERIFY_EVERY = 4,
    MAX_TIME = 10,
    MAX_STRIPS = 1,
    MAX_ADDS = 1,
    MAX_VERIFY_FAILS = 1,
    MAX_RECONCILE_FAILS = 2,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 1,
    TASK_TIMEOUT = 2
  ).*
}

// =========================================================================
// The OBSERVATION-LAG regime (bughunt-13 F5). The live module's
// holderChange FUSES the Lease transition with this pod's observation
// of it: the edge-mark fires in the same step the world moves, so the
// fused abstraction gives edge-caused divergence "no age window" BY
// THE FUSION — a premise, not a production fact. In production every
// edge mark() site is OBSERVATION-time (a renew round-trip after the
// apiserver moved), and in the partition/handover case the deposed
// ex-leader may observe nothing for a while: its stale leader label —
// load-bearing for the leader-only Service — is repaired either by
// its own lagged observation (the lose edge, one no-op-cost reconcile
// behind) or by the NEW holder's peer sweep, which the pre-F5 model
// declared environment WITHOUT pricing. This regime unfuses the two
// and prices the window:
//   - lagHolderChange moves the WORLD with no local mark (desire — and
//     therefore divergence — is judged against the actual Lease);
//   - observeEdge is the deferred local edge (the production mark
//     sites), clearing the lag;
//   - otherSweepsUs is the new holder's sweep stripping our stale
//     label (production sweep_peer_leader_marks from the peer's side)
//     — the partition-case repair, an environment action WITH the
//     repair obligation now pricing it;
//   - the lagTick repair cap bounds the stale-window: an unrepaired,
//     undiscovered divergence cannot age past REPAIR_BOUND (the new
//     holder's acquire-dirt reconcile lands within ~1–2 of its
//     rounds; our own observation within ~1 round-trip).
// lagDivergenceBounded is the RE-CHECKED bound: ByEdge divergence has
// exactly that window here — not "none". The lease-f5-unpriced-lag
// twin keeps the repairs enabled but UNFORCED (the pre-F5 "declared
// environment" posture: no repair obligation, no sweep pricing) and
// must violate it — repair-enabled is not repair-happening.
//
// SCOPE (recorded): the verify plane and both external falsifier
// polarities are out of this regime (budgets 0; verify absent from
// the step) — their bounds are the base regime's subject, and a
// verify mid-lag compares against a belief this regime deliberately
// splits from the world. Reconciles run observation-current only
// (the not(obsLag) gate): the stale-polarity write face is the live
// module's ByWrite subject, not the lag window's.
module leaderMarksObservationLag {
  import leaderMarks(
    VERIFY_EVERY = 4,
    MAX_TIME = 10,
    MAX_STRIPS = 0,
    MAX_ADDS = 0,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 2,
    MAX_HOLDER_CHANGES = 2,
    MAX_REBOUNDS = 0,
    TASK_TIMEOUT = 2
  ).*

  /// The stale-label repair budget in rounds: min(our own lagged
  /// observation — production one renew round-trip; the new holder's
  /// first successful reconcile+sweep — acquire dirt, spawn, patch:
  /// ~2 of its rounds). 3 carries one round of slack.
  val REPAIR_BOUND = 3

  /// An our-leadership-affecting world move not yet locally observed.
  var obsLag: bool

  /// The world's holder moves with NO local mark — the unfused half
  /// the live module's holderChange hides. Desire (and wrongness)
  /// move NOW; our edge fires at observeEdge.
  // Environment: the Lease changing hands at the apiserver, unfused from this pod's observation (the F5 window the live module's fused holderChange hides)
  action lagHolderChange(h: HolderState): bool = all {
    holderChanges < MAX_HOLDER_CHANGES,
    h != holder,
    holder' = h,
    obsLag' = (obsLag or leadingOf("us", h) != leadingOf("us", holder)),
    marked' = marked, cleared' = cleared,
    marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges + 1, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, h, wrongSince, clock),
    wrongCause' = stampCause(marks, h, wrongSince, wrongCause, ByEdge),
  }

  // Environment: the world's move, any target (nondet wrapper of lagHolderChange)
  action lagHolderChangeAny = {
    nondet h = Set(NoLeader, Leader("us"), Leader("other")).oneOf()
    lagHolderChange(h)
  }

  /// The deferred local edge: this pod's round-trip observes the
  /// moved Lease and runs the edge — including the mark the live
  /// module fuses into holderChange.
  // Mirrors: rio-lease/src/lib.rs:observe_held_while_believing
  action observeEdge = all {
    obsLag,
    obsLag' = false,
    marked' = marked + 1,
    cleared' = cleared,
    holder' = holder, marks' = marks, task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// The NEW holder's peer sweep strips OUR stale leader label — the
  /// partition-case repair (a deposed ex-leader cannot remove its
  /// own; production sweep_peer_leader_marks from the peer's side).
  // Environment: the new holder's own reconcile sweeping our stale label (sweep_peer_leader_marks from the peer pod — the partition-case repair the pre-F5 model left unpriced)
  action otherSweepsUs = all {
    holder == Leader("other"),
    marks.get("us"),
    marks' = marks.put("us", false),
    holder' = holder, marked' = marked, cleared' = cleared,
    task' = task, clock' = clock,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased, obsLag' = obsLag,
    wrongSince' = stampWrong(marks.put("us", false), holder, wrongSince, clock),
    wrongCause' = stampCause(marks.put("us", false), holder, wrongSince, wrongCause, NoCause),
  }

  /// One round passes, carrying THE REPAIR OBLIGATION: an unrepaired,
  /// undiscovered divergence of OUR marks cannot age past
  /// REPAIR_BOUND — the new holder's sweep cadence and our own
  /// observation round-trip are both inside it (production runs both
  /// loops by construction; the pre-F5 model priced neither).
  // Mirrors: rio-lease/src/lib.rs:run_lease_loop_with_client
  action lagTick = all {
    clock < MAX_TIME,
    (wrongUnder(marks, holder, "us") and not(dirty) and task == NoTask) implies
      (clock + 1 - wrongSince.get("us") <= REPAIR_BOUND),
    (task != NoTask) implies (clock + 1 - taskSince <= TASK_TIMEOUT),
    clock' = clock + 1,
    holder' = holder, marks' = marks, marked' = marked,
    cleared' = cleared, task' = task,
    lastVerify' = lastVerify, strips' = strips, adds' = adds,
    verifyFails' = verifyFails, reconcileFails' = reconcileFails,
    holderChanges' = holderChanges, rebounds' = rebounds,
    clobberErased' = clobberErased, obsLag' = obsLag,
    wrongSince' = stampWrong(marks, holder, wrongSince, clock + 1),
    wrongCause' = stampCause(marks, holder, wrongSince, wrongCause, NoCause),
  }

  /// Reconciles run observation-current only (see SCOPE).
  // Mirrors: rio-lease/src/lib.rs:maybe_spawn_leader_marks
  action lagReconcileSpawn = all {
    not(obsLag),
    reconcileSpawn,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action lagReconcilePatched = all {
    reconcilePatched,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:clear_through
  action lagReconcileClear = all {
    reconcileClear,
    obsLag' = obsLag,
  }
  // Mirrors: rio-lease/src/lib.rs:spawn_patch_leader_marks
  action lagReconcileFail = all {
    reconcileFail,
    obsLag' = obsLag,
  }

  // Environment: the regime's init — the live init plus the lag flag down
  action lagInit = all {
    init,
    obsLag' = false,
  }

  /// THE RE-CHECKED BOUND (F5): edge-caused divergence of OUR marks
  /// has exactly the REPAIR_BOUND window — not "none" (the fused
  /// abstraction's premise) and not "unbounded environment" (the
  /// pre-F5 partition posture). Discovered divergence (dirty/task)
  /// rides the live module's reconcile machinery as usual.
  val lagDivergenceBounded =
    wrongUnder(marks, holder, "us") implies or {
      dirty,
      task != NoTask,
      clock - wrongSince.get("us") <= REPAIR_BOUND,
    }

  /// Witness (expect-violation): the lag window actually opens — a
  /// stale-marked "us" while the world moved away is reachable with
  /// the lag pending (the regime's subject is exercised).
  val noStaleLagWindow = not(and {
    obsLag,
    wrongUnder(marks, holder, "us"),
  })

  action lagStep = any {
    lagTick,
    lagHolderChangeAny,
    observeEdge,
    otherSweepsUs,
    lagReconcileSpawn,
    lagReconcilePatched,
    lagReconcileClear,
    lagReconcileFail,
  }

  /// W13-M, end to end: acquire and converge (lagged observation),
  /// the world moves AWAY (no local mark — the window opens), the
  /// stale label lives one round undiscovered, and the NEW holder's
  /// sweep repairs it before our observation ever lands — the
  /// partition-case repair, priced.
  run lagWindowSweptRun =
    lagInit
      .then(lagHolderChange(Leader("us")))
      .then(observeEdge)
      .then(lagReconcileSpawn)
      .then(lagReconcilePatched)
      .then(lagReconcileClear)
      .then(lagHolderChange(Leader("other")))
      .then(lagTick)
      .then(otherSweepsUs)
      .expect(all {
        not(marks.get("us")),
        wrongSince.get("us") == -1,
        obsLag,
      })
      .then(observeEdge)
      .then(lagReconcileSpawn)
      .then(lagReconcilePatched)
      .then(lagReconcileClear)
      .expect(all {
        not(marks.get("us")),
        not(dirty),
        not(obsLag),
      })
}

// =========================================================================
// The CONVERGENCE regime (bughunt-13 F6): the horizon exceeds
// convergenceBound — (1+0)·((1+0)·3+2 + (1+1)·2) = 9 < MAX_TIME = 12 —
// so marksConvergenceBounded is FALSIFIABLE here, which the base
// regime cannot offer (its richer fault menu pushes the bound past its
// horizon; wiring the convergence val there would be a green nobody
// could ever lose — recorded at the quint.nix block). One add, one
// budgeted reconcile failure, no holder churn: the divergence must be
// discovered AND repaired through a failing attempt inside the bound.
// The lease-f6-fail-livelock twin raises the failure budget past the
// horizon and must violate — while the OLD dirty-disjunct form stays
// green over the same livelock (the discovery-not-repair kill).
module leaderMarksConvergence {
  import leaderMarks(
    VERIFY_EVERY = 3,
    MAX_TIME = 12,
    MAX_STRIPS = 0,
    MAX_ADDS = 1,
    MAX_VERIFY_FAILS = 0,
    MAX_RECONCILE_FAILS = 1,
    MAX_HOLDER_CHANGES = 0,
    MAX_REBOUNDS = 0,
    TASK_TIMEOUT = 2
  ).*
}
