// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F6 (teardown
// obligations): when a session ends — by any exit path — the gateway
// cancels every build the session started, reaps the session's tasks and
// sends the ordered close-out, and no teardown mechanism may drop those
// obligations (gw-session corpus §3 F6, design §3.4 GW-9/GW-10/GW-11/GW-20;
// verdicts in the VERDICT block at the end of this header). Same extend-and-override
// pattern as gw-f1-capacity.qnt.
//
// Candidates carried by this file:
//   - GW-9  (bbf30cc7f)  gwCalibF6WireErrorRemovesTracked -> s16
//   - GW-9  (0f476d6f0/a207ee15c) gwCalibF6ExitEdgeSkipsCancel -> s10
//   - GW-10 (2e54649c3)  gwCalibF6AbortWinsOverCancel     -> s10
//   - GW-11 (1cca275b4)  gwCalibF6FinishSkipsPumpReap     -> l8
//   - GW-11 (9a18f756a)  gwCalibF6ConnDropLeaksGauge      -> s3
//   - GW-20 (24bd41581)  gwCalibF6CloseOutSkipsExitStatus -> s19
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f6-teardown.qnt

// =========================================================================
// CALIBRATION — pre-bbf30cc7f (GW-9, tracking half): the build handler
// removed the build id from active_build_ids when the opcode returned
// REGARDLESS of outcome — including stream failures with no terminal
// outcome — so a disconnect mid-build found an empty set and the cancel
// loop was a no-op (the build leaked on the scheduler).
// PREDICTED: s16TrackedBuildPolicy falsifies at the first Wire-class
// stream failure that clears the tracking (a build leaves the tracked set
// with neither a processed terminal outcome nor a non-Wire error class).
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built keep-on-Wire policy; HOLDS).
// =========================================================================
//
// VERDICT (Stage-C calibration, relocated from the retired gw-session
// invariant map; scope/baseline protocol and the acceptance table:
// calibration/README.md, gateway section; trace walks verbatim from the
// Stage-C record):
// - GW-9 gwCalibF6WireErrorRemovesTracked (tracking cleared early):
//   FALSIFIES S16 @ 11 (6,966/403) — a tracked build's stream fails with a
//   Wire-class error and the pre-fix handler removes it from
//   active_build_ids anyway (bbf30cc7f: the disconnect cleanup loop later
//   finds an empty set).
// - GW-9 gwCalibF6ExitEdgeSkipsCancel (exit edge): FALSIFIES S10 @ 12
//   (14,698/723) — a non-panic exit edge skips the cancel loop with a build
//   still tracked (the 0f476d6f0 / a207ee15c case-completeness shape).
// - GW-10 gwCalibF6AbortWinsOverCancel: FALSIFIES S10 @ 11 (4,599/277) —
//   channel close hard-aborts the protocol task (the pre-fix
//   ChannelSession::Drop abort); the cancel obligation is destroyed with
//   it. GW-9's exit-edge arm and GW-10 falsify the same property through
//   different reverted mechanisms (a missing call site vs an abort racing
//   the cancel loop) — not the §1 simplification trigger.
// - GW-11 gwCalibF6FinishSkipsPumpReap (task quiescence): FALSIFIES L8 @ 14
//   (77,880/3,536) — the close-out completes but the pre-fix finish awaits
//   the client pump instead of reaping it (1cca275b4).
// - GW-11 gwCalibF6ConnDropLeaksGauge (gauge, 9a18f756a): FALSIFIES S3 @ 7
//   (965/100) — a connection drop tears down a still-held SessionGuard but
//   the pre-fix path never decrements channels_active; S2 stays true on the
//   same trace (the permit release is correct), so the falsification is
//   attributable to the gauge alone.
// - GW-20 gwCalibF6CloseOutSkipsExitStatus: FALSIFIES S19 @ 10 (3,467/222)
//   — the close-out ladder starts at eof (24bd41581); V-only per the §3.4
//   row (the client-side hang is environment, not modeled).
// - BASELINE: gwCalibF6AsBuilt (GW-9, GW-10, GW-11, GW-20) 3,253,999
//   distinct / 224 s — HOLDS; reachability-equivalent to famUpstream
//   (3,253,999 — exact structural cross-check).
module gwCalibF6WireErrorRemovesTracked {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX tracking removal: a Wire-class build-stream failure (the
  /// outcome class the shipped policy deliberately KEEPS tracked, because
  /// the client is gone and the cancel loop must cover it) clears the
  /// tracking on opcode return — no terminal outcome processed, no
  /// non-Wire classification.
  action calibWireErrorRemovesTracked(c: str, i: int): bool = all {
    LET_UPSTREAM,
    sessAt(c, i).exists, sessAt(c, i).proto == POpBuild, sessAt(c, i).tracked,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: POpEmit, tracked: false, streamHeld: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      rpcCompletesBuildSubmit(ADV, i),
      buildTerminal(ADV, i),
      calibWireErrorRemovesTracked(ADV, i),
      emitDone(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoHandlerError(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-0f476d6f0 / pre-a207ee15c (GW-9, exit-edge half): one
// of the session-exit paths (idle timeout, non-EOF opcode-read error)
// returned without running cancel_active_builds — the recurring
// case-completeness bug shape (fixed four separate times in the corpus).
// The pre-fix exit edge below takes a handler-error exit straight to the
// task end, skipping the cancel loop, with a build still tracked.
// PREDICTED: s10CancelOnSessionEnd falsifies at the first skipped-cancel
// exit (proto exited on a non-panic reason with a tracked build and no
// CancelBuild attempt).
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built protoExit routes every tracked exit through the cancel loop;
// HOLDS).
// =========================================================================
module gwCalibF6ExitEdgeSkipsCancel {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX handler-error exit edge: the proto task ends directly — no
  /// cancel loop — leaving the tracked build orphaned on the scheduler.
  action calibHandlerErrorSkipsCancel(c: str, i: int): bool = all {
    sessAt(c, i).exists,
    sessAt(c, i).proto == POpRead or sessAt(c, i).proto == POpRpc or
      sessAt(c, i).proto == POpBuild or sessAt(c, i).proto == POpEmit,
    or {
      sessAt(c, i).eg == EgCloseOut, sessAt(c, i).eg == EgDone, sessAt(c, i).eg == EgAborted,
      not(conn(c).live), conn(c).expired, chanAt(c, i).ch == ChClosed
    },
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: PExited, exitR: RHandlerErr,
      hsArmed: false, idleArmed: false, rpcDl: false, streamHeld: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      rpcCompletesBuildSubmit(ADV, i),
      buildTerminal(ADV, i),
      buildStreamErrorNonWire(ADV, i),
      emitDone(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      calibHandlerErrorSkipsCancel(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-2e54649c3 (GW-10): on TCP-RST russh fires channel_eof
// (the cancel loop starts) and channel_close (ChannelSession::Drop
// abort()s the task) concurrently — when the abort wins, the CancelBuild
// never reaches the scheduler. The pre-fix channel close below aborts the
// proto task outright, mid-cancel or pre-cancel.
// PREDICTED: s10CancelOnSessionEnd falsifies at the first abort-wins close
// (proto forced to exited with the build still tracked and no CancelBuild
// attempt).
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built peerChannelClose never aborts the proto task; HOLDS).
// =========================================================================
module gwCalibF6AbortWinsOverCancel {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX channel close: ChannelSession::Drop hard-aborts the protocol
  /// task — the cancel loop (running or about to run) is destroyed with
  /// it; bookkeeping and guard release are the as-built close path.
  action calibChannelCloseAbortsProto(c: str, i: int): bool = all {
    chanAt(c, i).ch == ChAccepted,
    conn(c).live, not(conn(c).expired), not(conn(c).herr), not(conn(c).discQ),
    sessAt(c, i).exists, sessAt(c, i).proto != PExited,
    val s = sessAt(c, i)
    val guardWasHeld = s.guard
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i),
        ch: ChClosed,
        writeHalf: false,
        sess: { ...s,
          proto: PExited,
          exitR: RClientEof,
          guard: false,
          eg: if (s.eg == EgFwd or s.eg == EgSendPend or s.eg == EgCloseOut) EgAborted else s.eg,
          hsArmed: false, idleArmed: false, rpcDl: false, streamHeld: false,
          sendBudget: false,
          coBudget: false,
          pump: false
        }
      }),
      conns' = conns.set(c,
        if (guardWasHeld)
          { ...connAfterGuardRelease(conn(c)), openCh: conn(c).openCh - 1 }
        else
          { ...conn(c), openCh: conn(c).openCh - 1 }),
      srv' = if (guardWasHeld)
          { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 }
        else srv,
      viol' = viol,
      reach' = reach
    }
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      rpcCompletesBuildSubmit(ADV, i),
      buildTerminal(ADV, i),
      emitDone(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoHandlerError(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      calibChannelCloseAbortsProto(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-1cca275b4 (GW-11, task-quiescence half):
// finish_channel_session awaited the client pump unbounded, and the pump
// only exits on client action — a client that never closes the channel
// parks the task and its buffers indefinitely, uncounted by any cap (the
// permit was already released).
// PREDICTED: l8SessionTaskQuiescence falsifies at the first completed
// finish with the pump still live (proto exited, finish done, session
// tasks not at zero).
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built closeOutComplete reaps the pump; HOLDS).
// =========================================================================
module gwCalibF6FinishSkipsPumpReap {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX finish completion: the close-out completes but the client
  /// pump is awaited, not reaped — it stays live until the client acts.
  action calibCloseOutCompleteNoReap(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut,
    sessAt(c, i).coStatus, sessAt(c, i).coEof, sessAt(c, i).coClose,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgDone, coBudget: false } }),
    conns' = conns, srv' = srv, viol' = viol,
    reach' = { ...reach, closeOutOrdered: true }
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      rpcCompletesStore(ADV, i),
      emitDone(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      closeOutSendStatus(ADV, i),
      closeOutSendEof(ADV, i),
      closeOutSendClose(ADV, i),
      calibCloseOutCompleteNoReap(ADV, i),
      closeOutDeadTransport(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-9a18f756a (GW-11, gauge half): the channels_active
// gauge decrement was scattered over three explicit call sites; abnormal
// exit paths (proto-task panic, connection drop) bypassed them and the
// gauge leaked. The pre-fix transport reap below releases the permit and
// the per-connection bookkeeping but never decrements the gauge for the
// guards it tears down.
// PREDICTED: s3GaugeAccuracy falsifies at the first connection drop that
// takes a still-held SessionGuard with it (gauge no longer equals live
// guards).
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built reapTransportEnd decrements per released guard; HOLDS).
// =========================================================================
module gwCalibF6ConnDropLeaksGauge {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX transport end: the connection-drop path bypasses the gauge
  /// decrement for the SessionGuards it destroys (the scattered explicit
  /// call sites did not cover this exit); permits and per-connection
  /// counters release as built.
  action calibReapLeaksGauge(c: str, letter: ReapLetter): bool = all {
    conn(c).exists, conn(c).live,
    match letter {
      | LPeerClose => true
      | LHandlerError => conn(c).herr
      | LDeadlineExpired => conn(c).expired
      | LKeepalive => keepaliveEnabled(c)
      | LTcpUserTimeout => tcpUtoEnabled(c)
      | LInactivity => inactivityEnabled(c)
    },
    val released = connGuards(c)
    all {
      conns' = conns.set(c, connOnDrop(conn(c))),
      chans' = CHKEYS.mapBy(k => if (k._1 == c) chanOnConnDrop(chans.get(k)) else chans.get(k)),
      srv' = { ...srv,
        connPermits: if (conn(c).permit) srv.connPermits - 1 else srv.connPermits,
        connsActive: if (conn(c).counted) srv.connsActive - 1 else srv.connsActive,
        sessPermits: srv.sessPermits - released
      },
      viol' = { ...viol,
        timerReapedCompliant: viol.timerReapedCompliant or
          (compliant(c) and (letter == LKeepalive or letter == LTcpUserTimeout or letter == LInactivity))
      },
      reach' = reach
    }
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      calibReapLeaksGauge(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-24bd41581 (GW-20): the close-out omitted the RFC 4254
// exit-status before eof/close — a ControlMaster foreground ssh waits for
// it forever (the client-side hang is environment, so this candidate has
// no T half; the gateway-side obligation is the ordered close-out).
// PREDICTED: s19CloseOutOrder falsifies at the first eof sent without
// exit-status.
// Constants: famUpstream scope. Baseline: gwCalibF6AsBuilt.baselineStep
// (as-built ladder sends exit-status first; HOLDS).
// =========================================================================
module gwCalibF6CloseOutSkipsExitStatus {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX close-out: the ladder starts at eof — exit-status is never
  /// sent.
  action calibCloseOutEofWithoutStatus(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, not(sessAt(c, i).coEof),
    conn(c).queue < QUEUE_DEPTH, conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), coEof: true } }),
    conns' = conns.set(c, { ...conn(c), queue: conn(c).queue + 1 }),
    srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      calibCloseOutEofWithoutStatus(ADV, i),
      closeOutSendClose(ADV, i),
      closeOutComplete(ADV, i),
      closeOutDeadTransport(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINE for this file (famUpstream-shaped union alphabet,
// same constants).
// =========================================================================
module gwCalibF6AsBuilt {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  action baselineStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      rpcCompletesStore(ADV, i),
      rpcCompletesBuildSubmit(ADV, i),
      buildTerminal(ADV, i),
      buildStreamErrorNonWire(ADV, i),
      emitDone(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoHandlerError(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      closeOutSendStatus(ADV, i),
      closeOutSendEof(ADV, i),
      closeOutSendClose(ADV, i),
      closeOutComplete(ADV, i),
      closeOutDeadTransport(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}
