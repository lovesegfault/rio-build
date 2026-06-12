// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F5 (egress flow
// control / bounded queues): data queued toward the client is bounded by
// the granted SSH window plus fixed-size queues, every send carries a
// budget, a send still pending at the budget is treated as a dead
// transport, and capacity release never depends on the peer draining the
// close-out (gw-session corpus §3 F5, design §3.4 GW-2/GW-8; verdicts in
// the VERDICT block at the end of this header).
// Same extend-and-override pattern as gw-f1-capacity.qnt. The GW-2
// trace-admission (T) half re-introduces the over-tight budget per design
// §2d/§5: the override removes the peer-class enabling condition, the
// structural stand-in for "budget too small for normal congestion".
//
// Candidates carried by this file:
//   - GW-2 (51123b2be)  gwCalibF5ReleaseAfterCloseOut    (V) -> s15
//   - GW-2 (861888876)  gwCalibF5NoSendBudget            (V) -> l2
//   - GW-2 (§2d T half) gwCalibF5BudgetIgnoresPeerClass  (T) -> p5
//   - GW-8 (deea04bbc)  gwCalibF5UnwindowedSend          (V) -> s13
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f5-egress.qnt

// =========================================================================
// CALIBRATION — pre-51123b2be (GW-2, ordering half): the close-out sends
// ran BEFORE the SessionGuard drop, so a peer that parks the handle queue
// pins the dead session's global permit (and every later one's) until the
// queue drains. The pre-fix pump exit below enters the close-out with the
// guard still held; release happens only when the close-out completes.
// PREDICTED: s15ReleaseBeforeCloseOut falsifies at the first close-out
// state with the guard still held.
// Constants: famWedge scope. Baseline: gwCalibF5AsBuilt.baselineStep
// (as-built guard-drop-first ordering; HOLDS).
// =========================================================================
//
// VERDICT (Stage-C calibration, relocated from the retired gw-session
// invariant map; scope/baseline protocol and the acceptance table:
// calibration/README.md, gateway section; trace walks verbatim from the
// Stage-C record):
// - GW-2 gwCalibF5ReleaseAfterCloseOut (release ordering): FALSIFIES S15 @
//   9 (5,228/408) — client EOF → proto exits → pre-fix pump exit enters the
//   close-out with the guard still held (51123b2be: release waited on the
//   peer-parkable handle queue).
// - GW-2 gwCalibF5NoSendBudget (send budget): FALSIFIES L2 @ 10 (6,600/479)
//   — a withhold-window peer parks the send with no HANDLE_SEND_TIMEOUT
//   armed; capacity held toward a transport-withholding peer with nothing
//   armed (861888876's unbounded handle.data() await).
// - GW-2 gwCalibF5BudgetIgnoresPeerClass (T): FALSIFIES P5 @ 10
//   (32,253/1,567) — a compliant peer's window runs out and the budget,
//   stripped of the §2d peer-class condition, fires the wedge response
//   against it (budgetFiredOnCompliant / fcByBudget — the structural form
//   of "budget too small for normal congestion", the 5 s close-out budget
//   861888876 replaced).
// - GW-8 gwCalibF5UnwindowedSend: FALSIFIES S13 @ 7 (922/117) — the first
//   response send goes through the pre-fix non-window path (Handle::data);
//   russh-side pending occupancy above zero with no credit consumed
//   (deea04bbc). L2 deliberately does not fall here (pre-fix sends complete
//   instantly into the unbounded buffer), exactly as the §3.4 row predicts.
// - BASELINE: gwCalibF5AsBuilt (GW-2 V+T, GW-8) 66,059 distinct / 44 s —
//   HOLDS; reachability-equivalent to famWedge (66,059 — exact structural
//   cross-check).
module gwCalibF5ReleaseAfterCloseOut {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX pump exit: the close-out begins with the SessionGuard still
  /// held (release ordered after the sends).
  action calibPumpExitEofReleaseLater(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PExited,
    sessAt(c, i).guard,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgCloseOut, coBudget: true } }),
    conns' = conns,
    srv' = srv,
    viol' = viol,
    reach' = reach
  }

  /// PRE-FIX close-out completion: only now does the guard drop (the
  /// pre-fix release point, after the peer accepted the sends).
  action calibCloseOutCompleteReleases(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut,
    sessAt(c, i).coStatus, sessAt(c, i).coEof, sessAt(c, i).coClose,
    sessAt(c, i).guard,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      eg: EgDone, coBudget: false, pump: false, guard: false } }),
    conns' = conns.set(c, connAfterGuardRelease(conn(c))),
    srv' = { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 },
    viol' = viol,
    reach' = { ...reach, closeOutOrdered: true }
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      pumpSendOk(ADV, 0),
      pumpSendBlocks(ADV, 0),
      peerWindowAdjust(ADV, 0),
      fireSendBudget(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoObservesConnGone(ADV, 0),
      calibPumpExitEofReleaseLater(ADV, 0),
      pumpExitSendFail(ADV, 0),
      closeOutSendStatus(ADV, 0),
      closeOutSendEof(ADV, 0),
      closeOutSendClose(ADV, 0),
      calibCloseOutCompleteReleases(ADV, 0),
      fireCloseOutBudget(ADV, 0),
      closeOutDeadTransport(ADV, 0),
      clientPumpSelfExit(ADV, 0),
      peerChannelClose(ADV, 0)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-861888876 (GW-2, budget half): the response pump did
// an UNBOUNDED handle.data() await while owning the SessionGuard — a peer
// that stops draining wedges every pump on the connection and pins
// permits/gauge/live-count with nothing armed to break the wait.
// PREDICTED: l2SendSettleArmed falsifies at the first send pending toward
// a transport-withholding peer with no budget armed (capacity held, no
// send budget, no close-out budget, no enforceable force-close).
// Constants: famWedge scope. Baseline: gwCalibF5AsBuilt.baselineStep
// (as-built pumpSendBlocks arms HANDLE_SEND_TIMEOUT; HOLDS).
// =========================================================================
module gwCalibF5NoSendBudget {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX window-exhausted send: the pump parks in the unbounded await
  /// — no HANDLE_SEND_TIMEOUT budget is armed for the pending send.
  action calibPumpSendBlocksNoBudget(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PHandshaking or sessAt(c, i).proto == POpEmit,
    chanAt(c, i).window == 0,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgSendPend } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      pumpSendOk(ADV, 0),
      calibPumpSendBlocksNoBudget(ADV, 0),
      peerWindowAdjust(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoObservesConnGone(ADV, 0),
      pumpExitEof(ADV, 0),
      pumpExitSendFail(ADV, 0),
      closeOutSendStatus(ADV, 0),
      closeOutSendEof(ADV, 0),
      closeOutSendClose(ADV, 0),
      closeOutComplete(ADV, 0),
      fireCloseOutBudget(ADV, 0),
      closeOutDeadTransport(ADV, 0),
      clientPumpSelfExit(ADV, 0),
      peerChannelClose(ADV, 0)
    }
  }
}

// =========================================================================
// CALIBRATION — GW-2 trace-admission (T) half, per design §2d/§5: the
// over-tight pre-861888876 budget shape ("5 s close-out budget abandoned
// close-outs under normal congestion, stranding clients"), re-introduced
// structurally by REMOVING the peer-class enabling condition from the
// send/close-out budget fire actions — the budget may fire against a
// compliant peer that is merely slow.
// PREDICTED: p5CompliantPeerNotForceClosed falsifies at the first budget
// firing against a compliant-class peer (the budgetFiredOnCompliant /
// fcByBudget latches).
// Constants: famWedge scope (the wedge letters stay available but the
// falsifying trace needs none of them). Baseline:
// gwCalibF5AsBuilt.baselineStep (as-built budgets fire only against
// transport-withholding peers; HOLDS).
// =========================================================================
module gwCalibF5BudgetIgnoresPeerClass {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX send-budget firing: no peer-class condition — a pending send
  /// toward ANY peer (including a compliant one whose window simply ran
  /// out under normal congestion) exhausts the budget and triggers the
  /// wedge response.
  action calibFireSendBudgetAnyPeer(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgSendPend, sessAt(c, i).sendBudget,
    sessAt(c, i).guard,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      eg: EgCloseOut, guard: false, sendBudget: false, coBudget: true } }),
    conns' = conns.set(c, { ...connAfterGuardRelease(conn(c)), fcArmed: true, fcByBudget: true }),
    srv' = { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 },
    viol' = { ...viol, budgetFiredOnCompliant: viol.budgetFiredOnCompliant or compliant(c) },
    reach' = { ...reach,
      stallFcArmed: true,
      releaseWithChannelOpen: reach.releaseWithChannelOpen or (chanAt(c, i).ch == ChAccepted and conn(c).live)
    }
  }

  /// PRE-FIX close-out-budget firing: same removal for the close-out
  /// budget — a pending close-out toward a compliant peer is abandoned and
  /// the force-close armed.
  action calibFireCloseOutBudgetAnyPeer(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, sessAt(c, i).coBudget,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgDone, coBudget: false, pump: false } }),
    conns' = conns.set(c, { ...conn(c), fcArmed: true, fcByBudget: true }),
    srv' = srv,
    viol' = { ...viol, budgetFiredOnCompliant: viol.budgetFiredOnCompliant or compliant(c) },
    reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      pumpSendOk(ADV, 0),
      pumpSendBlocks(ADV, 0),
      peerWindowAdjust(ADV, 0),
      calibFireSendBudgetAnyPeer(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoObservesConnGone(ADV, 0),
      pumpExitEof(ADV, 0),
      pumpExitSendFail(ADV, 0),
      closeOutSendStatus(ADV, 0),
      closeOutSendEof(ADV, 0),
      closeOutSendClose(ADV, 0),
      closeOutComplete(ADV, 0),
      calibFireCloseOutBudgetAnyPeer(ADV, 0),
      closeOutDeadTransport(ADV, 0),
      clientPumpSelfExit(ADV, 0),
      peerChannelClose(ADV, 0)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-deea04bbc (GW-8): session output was sent via
// Handle::data, which does no SSH window accounting — a client that
// withholds CHANNEL_WINDOW_ADJUST (TCP and keepalives healthy) absorbs the
// whole response stream into russh's unbounded per-channel pending buffer
// while every existing bound stays green.
// PREDICTED: s13WindowPacedEgress falsifies at the first non-window-path
// send (pending buffer occupancy above zero, no credit consumed).
// Constants: famWedge scope. Baseline: gwCalibF5AsBuilt.baselineStep
// (as-built sends are window-aware; HOLDS).
// =========================================================================
module gwCalibF5UnwindowedSend {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX response send: Handle::data — no window accounting, no
  /// stalling; the bytes land in russh's unbounded pending_data buffer
  /// regardless of the client-granted window.
  action calibPumpSendUnwindowed(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PHandshaking or sessAt(c, i).proto == POpEmit,
    chanAt(c, i).pending < PENDING_CEIL,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), pending: chanAt(c, i).pending + 1 }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      calibPumpSendUnwindowed(ADV, 0),
      peerWindowAdjust(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoObservesConnGone(ADV, 0),
      pumpExitEof(ADV, 0),
      pumpExitSendFail(ADV, 0),
      closeOutSendStatus(ADV, 0),
      closeOutSendEof(ADV, 0),
      closeOutSendClose(ADV, 0),
      closeOutComplete(ADV, 0),
      fireCloseOutBudget(ADV, 0),
      closeOutDeadTransport(ADV, 0),
      clientPumpSelfExit(ADV, 0),
      peerChannelClose(ADV, 0)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINE for this file (famWedge alphabet, same constants —
// the wired quint-gw-lifecycle-fam-wedge check is the same proof at
// LET_HOSTILE = false; this module re-runs it with the file's exact
// constant block so the baseline is attributable run-for-run).
// =========================================================================
module gwCalibF5AsBuilt {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  action baselineStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      pumpSendOk(ADV, 0),
      pumpSendBlocks(ADV, 0),
      peerWindowAdjust(ADV, 0),
      fireSendBudget(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoObservesConnGone(ADV, 0),
      pumpExitEof(ADV, 0),
      pumpExitSendFail(ADV, 0),
      closeOutSendStatus(ADV, 0),
      closeOutSendEof(ADV, 0),
      closeOutSendClose(ADV, 0),
      closeOutComplete(ADV, 0),
      fireCloseOutBudget(ADV, 0),
      closeOutDeadTransport(ADV, 0),
      clientPumpSelfExit(ADV, 0),
      peerChannelClose(ADV, 0)
    }
  }
}
