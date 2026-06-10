// -*- mode: Bluespec; -*-
//
// The cross-component ICE evidence/ack pipeline (bughunt-5 slot 5:
// bug_094 + merged_bug_134 + merged_bug_008 + merged_bug_003), at two
// cells: the controller's commit-on-Ack evidence buffer (per-cell
// ordered CellEvidence + the per-buffer epoch mint), a lossy
// serialized ack channel, and the scheduler's ICE backoff ladder
// (IceBackoff: per-cell step + mask + the per-cell last-applied
// epoch gate, plus the §13a local clear). This discharges the peer
// contract nodeclaimLifecycle.qnt explicitly assumes away
// ("Scheduler ICE ladder (peer): … backoff ladder … scheduler-side").
//
// PIPELINE LAWS UNDER VERIFICATION (the slot's three keystones):
//   - validate-then-commit: a REFUSED delivery applies NOTHING
//     (sched.sla.ack-validate-then-commit+1; pre-fix the cell planes
//     applied before the refusal — historically the since-retired
//     CostGateClosed class, merged_bug_046).
//   - epoch gate: the scheduler applies a cell event iff its epoch is
//     strictly greater than the cell's last-applied epoch — delivery
//     of already-applied evidence (redelivery `==`, reorder `<`) is a
//     TOTAL no-op (pre-fix: pinned masks, phantom climbs, inverted
//     clears — merged_bug_008's three axes).
//   - ordered buffer: a newer mark RETAINS an older buffered clear as
//     clear-then-mark; the request ships the cell in BOTH planes
//     (clear epoch < mark epoch) and the scheduler's fixed
//     clears-then-marks apply order realizes reset-then-step-0
//     (pre-fix: buffer_marks evicted the buffered clear —
//     merged_bug_003).
//
// CHANNEL: at most one request in flight, shipped BY READ from the
// retained buffer. Outcomes: delivered-Ok (applied + buffer drained),
// delivered-Ok-lost (applied, ack answer lost — buffer retained, a
// copy stashed as the network duplicate), refused (nothing applied,
// buffer retained), late-duplicate (the stashed copy applies after
// newer deliveries — the reorder lane).
//
// GHOST DISCIPLINE (how history properties become state invariants):
// each defect class latches a ghost boolean at the exact action where
// the defect would mutate state; the live actions write the ghosts
// with semantically-inert non-identity expressions (the documented
// quint-policy P4 allowance — the falsify twins are the teeth, P1).
//   errApplied        — some refusal mutated ladder state.
//   staleChanged      — an epoch-stale event mutated ladder state
//                       (re-stamp counts: a stale in-window mark is a
//                       mask extension even where the abstract state
//                       looks unchanged, so the no-gate apply marks it
//                       as a change by definition).
//   remaskedByStale   — an epoch-stale mark re-created a mask on a
//                       cell with no ladder entry (the post-local-
//                       clear re-mask, merged_bug_008 axis 3).
//   pendingCtm[c]     — the producer observed clear-then-mark for c
//                       and that chronology is not yet realized
//                       scheduler-side. Latched ONLY at the
//                       Clear→ClearThenMark buffer edge; cleared when
//                       a delivery's clear for c passes the gate (the
//                       reset realized).
//   ctmViolated       — a delivery for a pendingCtm cell left the
//                       ladder above step 0 (the destroyed-reset
//                       shape), or a realized clear-then-mark pair
//                       landed above step 0.
//
// SCOPE, deliberately narrow: epochs are the producer mint's counter
// (the SystemTime seed + max(now, prev+1) floor collapses to strict
// monotonicity — the only property the gate consumes); the epoch-less
// legacy lane is unit-tested in cost.rs and not modeled (the producer
// always mints post-fix; Q6 --wipe rollout); the observed-types plane
// and the binding snapshot have no cell-ladder interaction and are
// out of model; mask TTL is the masked/expired boolean (expiry
// nondeterministic — wall-clock-free).
//
// State counts, depth, and wall-clock: in the wired checks'
// transcripts and the introducing commit message, never here.
module iceEvidenceAck {
  /// Producer evidence events per trace (the mint ceiling; also the
  /// ladder-step saturation cap).
  const MAX_EVENTS: int

  type BufEv = BNone | BClear(int) | BMark(int) | BCtm((int, int))
  type Lad = LNone | LMasked(int) | LExpired(int)
  type OptE = ONone | OSome(int)
  type Planes = { clear: OptE, mark: OptE }
  type OptReq = RNone | RSome(str -> Planes)
  /// Per-cell apply outcome + the ghost bits the delivery actions
  /// fold (one record so the live and calibration compositions
  /// consume the SAME apply law). `cp` = clear gate-passed; `ma` =
  /// mark APPLIED (gate-passed, or force-applied in the no-gate
  /// defect lane) — a no-op'd stale mark sets neither.
  type AppRes = { lad: Lad, la: int, stale: bool, remask: bool, cp: bool, ma: bool }

  val CELLS = Set("c0", "c1")

  // --- controller ---------------------------------------------------------
  /// Per-cell ordered evidence buffer (PendingSchedulerEvidence).
  var buf: str -> BufEv
  /// The per-buffer epoch mint (strictly monotone; epochs from 1).
  var mint: int

  // --- channel --------------------------------------------------------------
  /// The at-most-one in-flight AckSpawnedIntents request.
  var inflight: OptReq
  /// The network duplicate a delivered-Ok-lost leaves behind (the
  /// late-duplicate / reorder lane).
  var stash: OptReq

  // --- scheduler ------------------------------------------------------------
  /// IceBackoff.cells — per-cell ladder entry.
  var lad: str -> Lad
  /// IceBackoff.last_applied — the per-cell epoch gate.
  var la: str -> int

  // --- ghosts ---------------------------------------------------------------
  var errApplied: bool
  var staleChanged: bool
  var remaskedByStale: bool
  var pendingCtm: str -> bool
  var ctmViolated: bool

  // --- pure law -------------------------------------------------------------

  /// The wire projection of one buffered cell (CellEvidence::planes).
  pure def planesOf(b: BufEv): Planes = match b {
    | BNone => { clear: ONone, mark: ONone }
    | BClear(e) => { clear: OSome(e), mark: ONone }
    | BMark(e) => { clear: ONone, mark: OSome(e) }
    | BCtm(p) => { clear: OSome(p._1), mark: OSome(p._2) }
  }

  pure def hasEntry(l: Lad): bool = match l {
    | LNone => false
    | LMasked(_) => true
    | LExpired(_) => true
  }

  pure def stepAbove0(l: Lad): bool = match l {
    | LNone => false
    | LMasked(s) => s > 0
    | LExpired(s) => s > 0
  }

  /// next_mark_step + the mask transition (the UNCHANGED ladder
  /// table): first mark masks at step 0; an in-window mark refreshes
  /// at the same rung; a post-expiry mark climbs (saturating).
  pure def markStep(l: Lad, cap: int): Lad = match l {
    | LNone => LMasked(0)
    | LMasked(s) => LMasked(s)
    | LExpired(s) => LMasked(if (s + 1 > cap) cap else s + 1)
  }

  /// One cell's apply: clears before marks (the fixed wire order),
  /// each through the epoch gate when `gate` is true. `gate = false`
  /// is the as-built pre-epoch law (apply everything) — the falsify
  /// twins' lane; its stale/remask bits report what the defect
  /// mutated (a stale in-window mark counts as a change BY
  /// DEFINITION: the real re-stamp extends the mask even though the
  /// abstract rung is unchanged).
  pure def applyCellG(gate: bool, l0: Lad, a0: int, p: Planes, cap: int): AppRes =
    val c = match p.clear {
      | ONone => { lad: l0, la: a0, stale: false, remask: false, cp: false, ma: false }
      | OSome(e) =>
        if (e > a0) { lad: LNone, la: e, stale: false, remask: false, cp: true, ma: false }
        else if (gate) { lad: l0, la: a0, stale: false, remask: false, cp: false, ma: false }
        else { lad: LNone, la: a0, stale: hasEntry(l0), remask: false, cp: false, ma: false }
    }
    match p.mark {
      | ONone => c
      | OSome(e) =>
        if (e > c.la) { ...c, lad: markStep(c.lad, cap), la: e, ma: true }
        else if (gate) c
        else { ...c,
          lad: markStep(c.lad, cap),
          stale: true,
          ma: true,
          remask: (c.remask or not(hasEntry(c.lad))) }
    }

  /// The clear-then-mark realization check for one delivered cell:
  /// a gate-passing clear paired with a mark MUST land step 0; a
  /// delivery that APPLIES a mark for a cell whose clear-then-mark
  /// chronology is pending MUST NOT leave the ladder above step 0
  /// (the chronology's reset must land before its mark — a stale
  /// NO-OP'd duplicate applies nothing and is exempt by the `ma`
  /// bit).
  pure def ctmViolCell(pending: bool, p: Planes, res: AppRes): bool =
    val pairBad = (res.cp and res.ma and res.lad != LMasked(0))
    val pendingBad = (pending and res.ma and stepAbove0(res.lad))
    (pairBad or pendingBad)

  pure def reqOf(o: OptReq): str -> Planes = match o {
    | RSome(r) => r
    | RNone => Set("c0", "c1").mapBy(c => { clear: ONone, mark: ONone })
  }

  // --- producer actions -------------------------------------------------------

  /// Registered=True edge: any buffered state supersedes to Clear
  /// (the merged_bug_005 direction, unchanged). Guard: the reconciler
  /// tick is single-threaded — evidence is never buffered while an
  /// Ack RPC is outstanding (request build and the answer are atomic
  /// w.r.t. the buffer; only the ANSWER can be lost or late).
  action obsClear(c: str): bool = all {
    inflight == RNone,
    mint < MAX_EVENTS,
    mint' = mint + 1,
    buf' = buf.set(c, BClear(mint + 1)),
    pendingCtm' = pendingCtm,
    inflight' = inflight, stash' = stash, lad' = lad, la' = la,
    errApplied' = errApplied, staleChanged' = staleChanged,
    remaskedByStale' = remaskedByStale, ctmViolated' = ctmViolated,
  }

  /// ICE mark (reap production site): the ordered-buffer law — a
  /// newer mark RETAINS an older buffered clear as clear-then-mark
  /// (the merged_bug_003 fix). The pendingCtm ghost latches at the
  /// Clear→ClearThenMark edge ONLY (an already-latched or realized
  /// chronology keeps its flag).
  action obsMark(c: str): bool = all {
    inflight == RNone,
    mint < MAX_EVENTS,
    mint' = mint + 1,
    buf' = buf.set(c, match buf.get(c) {
      | BNone => BMark(mint + 1)
      | BMark(_) => BMark(mint + 1)
      | BClear(e) => BCtm((e, mint + 1))
      | BCtm(p) => BCtm((p._1, mint + 1))
    }),
    pendingCtm' = pendingCtm.set(c, match buf.get(c) {
      | BClear(_) => true
      | BCtm(_) => pendingCtm.get(c)
      | BNone => pendingCtm.get(c)
      | BMark(_) => pendingCtm.get(c)
    }),
    inflight' = inflight, stash' = stash, lad' = lad, la' = la,
    errApplied' = errApplied, staleChanged' = staleChanged,
    remaskedByStale' = remaskedByStale, ctmViolated' = ctmViolated,
  }

  // --- channel actions --------------------------------------------------------

  /// Build the request FROM the buffer (ship by read — the buffer is
  /// retained until Ack-Ok).
  action ship: bool = all {
    inflight == RNone,
    CELLS.exists(c => buf.get(c) != BNone),
    inflight' = RSome(CELLS.mapBy(c => planesOf(buf.get(c)))),
    buf' = buf, mint' = mint, stash' = stash, lad' = lad, la' = la,
    errApplied' = errApplied, staleChanged' = staleChanged,
    remaskedByStale' = remaskedByStale, ctmViolated' = ctmViolated,
    pendingCtm' = pendingCtm,
  }

  /// Apply the in-flight request (the validate-then-commit COMMIT
  /// half — by construction nothing here can refuse). `drain` is the
  /// Ack-Ok arm reaching the controller (drain_delivered: planes
  /// reset, mint retained); `keepDup` leaves the network duplicate
  /// for the late-duplicate lane.
  action deliverApply(gate: bool, drain: bool, keepDup: bool): bool = all {
    inflight != RNone,
    val r = reqOf(inflight)
    val res = CELLS.mapBy(c => applyCellG(gate, lad.get(c), la.get(c), r.get(c), MAX_EVENTS))
    all {
      lad' = CELLS.mapBy(c => res.get(c).lad),
      la' = CELLS.mapBy(c => res.get(c).la),
      staleChanged' = (staleChanged or CELLS.exists(c => res.get(c).stale)),
      remaskedByStale' = (remaskedByStale or CELLS.exists(c => res.get(c).remask)),
      ctmViolated' = (ctmViolated or CELLS.exists(c => ctmViolCell(pendingCtm.get(c), r.get(c), res.get(c)))),
      // Realization: an APPLYING delivery that carried the cell's
      // clear plane realizes the chronology — now (gate-passing
      // clear) or earlier (stale clear: its reset applied on the
      // delivery whose Ok answer was lost; the paired mark then
      // landed on the post-reset rung). Mark-only deliveries never
      // clear the flag — that is exactly the defect signature.
      pendingCtm' = CELLS.mapBy(c => if (r.get(c).clear != ONone) false else pendingCtm.get(c)),
      buf' = if (drain) CELLS.mapBy(c => BNone) else buf,
      inflight' = RNone,
      stash' = if (keepDup) inflight else stash,
      errApplied' = (errApplied or false),
      mint' = mint,
    }
  }

  /// Ack delivered and the Ok answer observed: apply + drain.
  action deliverOk = deliverApply(true, true, false)

  /// Ack delivered, applied server-side, but the Ok answer is LOST
  /// (the routine client-timeout-after-server-apply): the controller
  /// retains its buffer and will redeliver; the network keeps a
  /// duplicate.
  action deliverOkLost = deliverApply(true, false, true)

  /// The whole-request refusal (deposed / undecodable entry / skewed
  /// arming echo — sched.sla.ack-validate-then-commit+1; the closed-
  /// cost-gate cause is retired, merged_bug_046): NOTHING
  /// is applied; the controller's buffer survives to redeliver. The
  /// ghost write is the live law stated as state (inert non-identity
  /// per the P4 allowance; the apply-before-refuse twin is the
  /// teeth).
  action refuse: bool = all {
    inflight != RNone,
    inflight' = RNone,
    errApplied' = (errApplied or false),
    buf' = buf, mint' = mint, stash' = stash, lad' = lad, la' = la,
    staleChanged' = staleChanged, remaskedByStale' = remaskedByStale,
    ctmViolated' = ctmViolated, pendingCtm' = pendingCtm,
  }

  /// The stashed network duplicate lands LATE — after newer
  /// deliveries (the reorder lane; epochs `<` last-applied).
  action lateDuplicate(gate: bool): bool = all {
    stash != RNone,
    val r = reqOf(stash)
    val res = CELLS.mapBy(c => applyCellG(gate, lad.get(c), la.get(c), r.get(c), MAX_EVENTS))
    all {
      lad' = CELLS.mapBy(c => res.get(c).lad),
      la' = CELLS.mapBy(c => res.get(c).la),
      staleChanged' = (staleChanged or CELLS.exists(c => res.get(c).stale)),
      remaskedByStale' = (remaskedByStale or CELLS.exists(c => res.get(c).remask)),
      ctmViolated' = (ctmViolated or CELLS.exists(c => ctmViolCell(pendingCtm.get(c), r.get(c), res.get(c)))),
      // Same realization law as deliverApply.
      pendingCtm' = CELLS.mapBy(c => if (r.get(c).clear != ONone) false else pendingCtm.get(c)),
      stash' = RNone,
      inflight' = inflight, buf' = buf, mint' = mint,
      errApplied' = (errApplied or false),
    }
  }

  // --- scheduler-local actions ------------------------------------------------

  /// The §13a first-pull local clear: removes the ladder entry,
  /// RETAINS last_applied (merged_bug_008 axis 3 — the redelivered
  /// retained mark must no-op, not re-mask the proven-healthy cell).
  action localClear(c: str): bool = all {
    hasEntry(lad.get(c)),
    lad' = lad.set(c, LNone),
    la' = la, buf' = buf, mint' = mint, inflight' = inflight,
    stash' = stash, errApplied' = errApplied,
    staleChanged' = staleChanged, remaskedByStale' = remaskedByStale,
    ctmViolated' = ctmViolated, pendingCtm' = pendingCtm,
  }

  /// Mask TTL expiry (entry retained — the step survives so a
  /// post-expiry mark climbs; only clears reset).
  action maskExpire(c: str): bool = all {
    match lad.get(c) {
      | LMasked(_) => true
      | LNone => false
      | LExpired(_) => false
    },
    lad' = lad.set(c, match lad.get(c) {
      | LMasked(s) => LExpired(s)
      | LNone => LNone
      | LExpired(s) => LExpired(s)
    }),
    la' = la, buf' = buf, mint' = mint, inflight' = inflight,
    stash' = stash, errApplied' = errApplied,
    staleChanged' = staleChanged, remaskedByStale' = remaskedByStale,
    ctmViolated' = ctmViolated, pendingCtm' = pendingCtm,
  }

  // --- harness ------------------------------------------------------------------

  action init = all {
    buf' = CELLS.mapBy(c => BNone),
    mint' = 0,
    inflight' = RNone,
    stash' = RNone,
    lad' = CELLS.mapBy(c => LNone),
    la' = CELLS.mapBy(c => 0),
    errApplied' = false,
    staleChanged' = false,
    remaskedByStale' = false,
    pendingCtm' = CELLS.mapBy(c => false),
    ctmViolated' = false,
  }

  action step = {
    nondet c = CELLS.oneOf()
    any {
      obsClear(c),
      obsMark(c),
      ship,
      deliverOk,
      deliverOkLost,
      refuse,
      lateDuplicate(true),
      localClear(c),
      maskExpire(c),
    }
  }

  // --- invariants -----------------------------------------------------------------

  /// A refusal applies nothing (validate-then-commit; bug_094 +
  /// merged_bug_008's apply-before-refuse axis).
  val errImpliesNoMutation = not(errApplied)

  /// Delivery of already-applied evidence (all epochs <= the cells'
  /// last-applied) changes nothing — redelivery and reorder are total
  /// no-ops (merged_bug_008 axes 1/2/4: no pinned mask, no phantom
  /// climb, no stale re-stamp).
  val redeliveryIdempotent = not(staleChanged)

  /// A producer-observed clear-then-mark chronology lands as
  /// reset-then-step-0, never a climb from the stale rung
  /// (merged_bug_003).
  val clearThenMarkRealizesReset = not(ctmViolated)

  /// After the §13a local clear, only STRICTLY NEWER evidence
  /// re-masks the cell (merged_bug_008 axis 3 — the inverted clear).
  val healthyCellNeedsNewEvidence = not(remaskedByStale)
}

/// The wired regime: small-but-saturating event budget (epochs 1..4
/// across 2 cells reach every gate arm: fresh, ==, <, post-local-
/// clear, and the clear-then-mark pair with room for a prior rung).
module iceEvidenceAckBase {
  import iceEvidenceAck(MAX_EVENTS = 4).*
}
