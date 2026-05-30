// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F1 (capacity
// conservation): connection permits, session permits and gauges equal the
// number of live connections/sessions, released exactly once on every
// ending, never dependent on peer cooperation (gw-session corpus §3 F1,
// design §3.4; verdicts in the calibration table of
// docs/spec/models/gw-session-invariant-map.md). Same extend-and-override
// pattern as the round-1 calibration modules: each module instantiates the
// as-built gwConnLifecycle core at a named regime/family scope, swaps ONE
// action for its PRE-FIX behavior (the behavior the named historical fix
// removed), and exposes it through `calibStep`. Violation latches stay
// as-built, so a falsification means the as-built invariant set re-finds
// that bug class. Baselines (`baselineStep` in the gwCalibF1AsBuilt*
// modules) restore the as-built action over the same alphabet and
// constants and must HOLD the same property.
//
// Candidates carried by this file:
//   - GW-1  (443670d43)  gwCalibF1ServerSideKeepsGuard      -> l2SendSettleArmed
//   - GW-12 (a2fcb8aa0)  gwCalibF1AuthRejectSkipsGate       -> s1ConnPermitConservation
//   - GW-18 (9d80038cb)  gwCalibF1NoGlobalSessionCap   (V)  -> s2SessionPermitConservation
//   - GW-18 (9d80038cb)  gwCalibF1PerConnCapRefusals   (T)  -> p1 / p2 / p3
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f1-capacity.qnt

// =========================================================================
// CALIBRATION — pre-443670d43 (GW-1): the session permit / gauge / grace
// were keyed on the per-connection `sessions` map entry, which only CLIENT
// action (channel close) removes. A protocol session that ends server-side
// (handshake timeout, idle timeout, protocol error) keeps its permit and
// gauge slot and never re-arms the empty grace; an exec-then-silent client
// pins max_sessions for everyone. The pre-fix pump exit below runs the
// close-out but never drops the SessionGuard — release happens only at
// peerChannelClose, exactly the pre-fix keying.
// PREDICTED: l2SendSettleArmed falsifies once the close-out completes with
// the guard still held (capacity held, nothing armed to release it without
// a client letter). S15 falls on the same trace (the ordering rule
// post-dates the pre-fix world); S2/S3 do NOT fall — the pre-fix accounting
// is internally consistent, the leak is a settlement failure.
// Constants: famAdmission scope (LET_OCC for the server-side ending) plus
// the close-out ladder. Baseline: gwCalibF1AsBuilt.baselineStep (HOLDS).
// =========================================================================
module gwCalibF1ServerSideKeepsGuard {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = true,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX pump exit on a server-side-ended session: the close-out is
  /// attempted as built, but the SessionGuard is NOT dropped (no permit /
  /// gauge / live-count release, no grace re-arm) — that release stays
  /// keyed on the client's channel close.
  action calibPumpExitEofKeepsGuard(c: str, i: int): bool = all {
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

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      peerWithholdsProgress(c),
      graceCheckPass(c),
      graceFire(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      fireOpcodeIdle(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      calibPumpExitEofKeepsGuard(ADV, i),
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

// =========================================================================
// CALIBRATION — pre-a2fcb8aa0 (GW-12): auth_publickey_offered skipped
// mark_real_connection / ensure_permit on the reject path. A client that
// skips the `none` probe and offers only unauthorized keys reaches the
// auth layer uncounted and, when over cap, is never torn down — the
// connection cap does not bound it.
// PREDICTED: s1ConnPermitConservation falsifies at the first reject-path
// auth callback (a live connection past an auth callback that is neither
// counted nor being torn down).
// Constants: the §2e cap-regime value MAX_CONNECTIONS = 1 (the GW-12
// override row of the structural-ceiling vs policy-cap table), LET_OCC for
// the reject-only peer. Baseline: gwCalibF1AsBuiltCap1.baselineStep
// (as-built peerAuthFail at the same constants/alphabet, HOLDS).
// =========================================================================
module gwCalibF1AuthRejectSkipsGate {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 1,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = true,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX reject-path auth callback: neither mark_real_connection nor
  /// ensure_permit runs — the connection is not counted, the gauge is not
  /// touched, and a permit-less (over-cap) connection is NOT torn down.
  action calibAuthFailSkipsGate(c: str): bool = all {
    c == ADV,
    conn(c).exists, conn(c).live, conn(c).banner, not(conn(c).kex),
    conn(c).stage == CTcp or conn(c).stage == CAuthAtt,
    not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conns' = conns.set(c, { ...conn(c), stage: CAuthAtt, occW: true }),
    srv' = srv,
    chans' = chans,
    viol' = viol,
    reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      calibAuthFailSkipsGate(c),
      peerAuthOk(c),
      peerSilent(c),
      peerWithholdsProgress(c),
      firePreAuthPhase1(c),
      firePreAuthPhase2(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-9d80038cb, safety half (GW-18 V): the capacity bound
// was per-connection only; nothing bounded the TOTAL number of live
// protocol sessions across connections, so an attacker spreading sessions
// over many connections was unbounded. The pre-fix exec admission below
// drops the global session-cap check.
// PREDICTED: s2SessionPermitConservation falsifies once held session
// permits exceed MAX_SESSIONS (three concurrent guards across the two
// connections at the §2e cap of 2).
// Constants: standard regime values; both connections may admit sessions
// (the spread shape). Baseline: gwCalibF1AsBuiltSpread.baselineStep (HOLDS).
// =========================================================================
module gwCalibF1NoGlobalSessionCap {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX exec admission: no global session-semaphore acquire — the
  /// admission proceeds whenever the channel-level preconditions hold,
  /// regardless of how many sessions are live across the instance.
  action calibExecAdmitNoGlobalCap(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChAccepted,
    chanAt(c, i).writeHalf,
    not(sessAt(c, i).exists),
    val wasArmed = conn(c).grace == GArmed
    all {
      conns' = conns.set(c, { ...conn(c),
        liveSess: conn(c).liveSess + 1,
        everSess: true,
        grace: if (wasArmed) GUnarmed else conn(c).grace
      }),
      chans' = chans.set((c, i), { ...chanAt(c, i), writeHalf: false, execs: chanAt(c, i).execs + 1, sess: admittedSess }),
      srv' = { ...srv, sessPermits: srv.sessPermits + 1, chansActive: srv.chansActive + 1 },
      viol' = viol,
      reach' = { ...reach,
        execDisarmedGrace: reach.execDisarmedGrace or wasArmed,
        muxReExec: reach.muxReExec or conn(c).sawZero
      }
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
      peerOpenChannel(c, i),
      calibExecAdmitNoGlobalCap(c, i),
      peerClientEof(c, i),
      protoSeesClientEof(c, i),
      protoObservesConnGone(c, i),
      pumpExitEof(c, i),
      pumpExitSendFail(c, i),
      clientPumpSelfExit(c, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-9d80038cb, permissiveness half (GW-18 T): the
// over-tight guards the fix removed, re-introduced. Three arms of the same
// pre-fix design at model scale (per-connection cap of 1):
//   - an exec on a connection that already has a live session is refused
//     even though global capacity remains (the ControlMaster fan-out
//     refusal);
//   - a channel open beyond the per-connection session count is refused at
//     the open (OpenSSH silent-fallback shape) instead of exec-rejected;
//   - the connection is disconnected the instant its live-session count
//     touches zero (kills mux masters between builds).
// PREDICTED: p1MuxFanOutAdmitted, p3CapRejectsExecOnly and
// p2SurvivesSessionCountZero each falsify (one run per property).
// Constants: standard regime values. Baseline:
// gwCalibF1AsBuiltSpread.baselineStep (as-built admission/close at the
// same constants, HOLDS all three).
// =========================================================================
module gwCalibF1PerConnCapRefusals {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX per-connection cap at exec: a second concurrent session on
  /// one connection is refused even though the global cap has room.
  action calibExecRefusedAtPerConnCap(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChAccepted,
    chanAt(c, i).writeHalf,
    not(sessAt(c, i).exists),
    conn(c).liveSess >= 1,
    srv.sessPermits < MAX_SESSIONS,
    viol' = { ...viol, execRefusedBelowCap: true },
    conns' = conns, chans' = chans, srv' = srv, reach' = reach
  }

  /// PRE-FIX per-connection cap at channel open: the open itself is
  /// refused once the connection has a live session (capacity signalled at
  /// the wrong protocol point).
  action calibOpenRefusedAtPerConnCap(c: str): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    conn(c).liveSess >= 1,
    srv.sessPermits < MAX_SESSIONS,
    viol' = { ...viol, capRefusedOpenOrConn: true },
    conns' = conns, chans' = chans, srv' = srv, reach' = reach
  }

  /// PRE-FIX disconnect-on-last-channel-close: the close that takes the
  /// live-session count to zero queues a disconnect immediately (no empty
  /// grace), killing the mux master between builds.
  action calibDisconnectOnLastClose(c: str, i: int): bool = all {
    chanAt(c, i).ch == ChAccepted,
    conn(c).live, not(conn(c).expired), not(conn(c).herr), not(conn(c).discQ),
    sessAt(c, i).exists, sessAt(c, i).guard,
    conn(c).liveSess == 1,
    val s = sessAt(c, i)
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i),
        ch: ChClosed,
        writeHalf: false,
        sess: { ...s,
          guard: false,
          eg: if (s.eg == EgFwd or s.eg == EgSendPend or s.eg == EgCloseOut) EgAborted else s.eg,
          sendBudget: false,
          coBudget: false
        }
      }),
      conns' = conns.set(c, { ...connAfterGuardRelease(conn(c)),
        openCh: conn(c).openCh - 1,
        grace: GUnarmed,
        discQ: true,
        fcArmed: true
      }),
      srv' = { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 },
      viol' = { ...viol, disconnectOnTouchZero: true },
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
      peerOpenChannel(c, i),
      execAdmit(c, i),
      execRejectCap(c, i),
      calibExecRefusedAtPerConnCap(c, i),
      calibOpenRefusedAtPerConnCap(c),
      calibDisconnectOnLastClose(c, i),
      peerClientEof(c, i),
      protoSeesClientEof(c, i),
      protoObservesConnGone(c, i),
      pumpExitEof(c, i),
      clientPumpSelfExit(c, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINES for this file. Each baselineStep restores the
// as-built action(s) over the same constants and the same restricted
// alphabet shape as the override(s) it baselines (one rich session
// dimension per module — the same principle as the wired family checks,
// so the exhaustive run stays minutes-class); it must HOLD the targeted
// properties for the falsifications to be attributable to the reverted
// behavior. gwCalibF1AsBuilt baselines GW-1 (conn-A sessions, occupancy
// letters, the close-out ladder); gwCalibF1AsBuiltSpread baselines the two
// GW-18 halves (both connections admit sessions, minimal in-session
// alphabet); gwCalibF1AsBuiltCap1 baselines GW-12 (cap-1 constants, the
// as-built reject-path callback).
// =========================================================================
module gwCalibF1AsBuilt {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = true,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
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
      peerWithholdsProgress(c),
      graceCheckPass(c),
      graceFire(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      fireOpcodeIdle(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
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

module gwCalibF1AsBuiltSpread {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
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
      peerOpenChannel(c, i),
      execAdmit(c, i),
      execRejectCap(c, i),
      peerClientEof(c, i),
      protoSeesClientEof(c, i),
      protoObservesConnGone(c, i),
      pumpExitEof(c, i),
      pumpExitSendFail(c, i),
      clientPumpSelfExit(c, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter)
    }
  }
}

module gwCalibF1AsBuiltCap1 {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 1,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = true,
    LET_WEDGE = false,
    LET_HOSTILE = false,
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
      peerAuthFail(c),
      peerAuthOk(c),
      peerSilent(c),
      peerWithholdsProgress(c),
      firePreAuthPhase1(c),
      firePreAuthPhase2(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter)
    }
  }
}
