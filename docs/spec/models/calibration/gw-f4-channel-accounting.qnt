// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F4 (bookkeeping
// integrity of channel/session counts): the counters admission decisions
// read equal the underlying truth — only accepted opens count, only
// matching closes decrement, the bound is enforced at the allocation
// transition, and out-of-contract opens terminate the connection so
// untracked russh-side state cannot grow (gw-session corpus §3 F4, design
// §3.4 GW-5/GW-6/GW-7; verdicts in the calibration table of
// docs/spec/models/gw-session-invariant-map.md). Same extend-and-override
// pattern as gw-f1-capacity.qnt.
//
// Candidates carried by this file:
//   - GW-5 (9739aca65)            gwCalibF4ForgedCloseDecrements -> s7
//   - GW-6 (719f99809)            gwCalibF4BoundOnSessionsMap    -> s7
//   - GW-7 (2cd23d940/eef1fbd8d)  gwCalibF4RefusePolitely        -> s14
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f4-channel-accounting.qnt

// =========================================================================
// CALIBRATION — pre-9739aca65 (GW-5): russh dispatches callbacks for any
// well-formed channel id; channel_close decremented `open_channels`
// unconditionally, and exec was honored on channels that were never
// accepted. Forged/duplicate closes hold the counter near zero while
// russh-side state grows, and rogue execs spawn sessions on never-accepted
// channels.
// PREDICTED: s7ChannelAccounting falsifies at the first forged-close
// decrement (counter no longer equals the accepted-open set; an unguarded
// decrement on an idle connection also drives it negative) or at the first
// exec on a never-accepted channel (the latch half).
// Constants: standard regime values, LET_HOSTILE for the forged-close
// letter. Baseline: gwCalibF4AsBuilt.baselineStep (HOLDS).
// =========================================================================
module gwCalibF4ForgedCloseDecrements {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX channel_close handling: the decrement is unconditional — a
  /// close for an id that was never accepted (forged or duplicate) still
  /// decrements open_channels.
  action calibForgedCloseDecrements(c: str): bool = all {
    c == ADV,
    conn(c).exists, conn(c).live, not(conn(c).herr), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), openCh: conn(c).openCh - 1 }),
    chans' = chans, srv' = srv, viol' = viol,
    reach' = reach
  }

  /// PRE-FIX exec gate: an exec is honored on a channel whose open was
  /// refused or never happened (no accepted-set membership check); it
  /// consumes a session permit and spawns a session on russh-side state
  /// the bookkeeping never admitted.
  action calibExecOnUnacceptedChannel(c: str, i: int): bool = all {
    c == ADV,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChNone,
    not(sessAt(c, i).exists),
    srv.sessPermits < MAX_SESSIONS,
    conns' = conns.set(c, { ...conn(c), liveSess: conn(c).liveSess + 1, everSess: true }),
    chans' = chans.set((c, i), { ...chanAt(c, i), execs: chanAt(c, i).execs + 1, sess: admittedSess }),
    srv' = { ...srv, sessPermits: srv.sessPermits + 1, chansActive: srv.chansActive + 1 },
    viol' = { ...viol, execOnNonOpenChannel: true },
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
      peerOpenChannel(c, i),
      execAdmit(ADV, i),
      calibForgedCloseDecrements(c),
      calibExecOnUnacceptedChannel(c, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-719f99809 (GW-6): the per-connection channel bound was
// checked at channel_open against the exec'd-sessions map (`sessions.len()`)
// while the resource is allocated at the open — a burst of N opens (each
// seeing len()==0) followed by N execs defeats the bound.
// PREDICTED: s7ChannelAccounting falsifies once the open count exceeds the
// per-connection bound (two accepted opens at the override's policy cap of
// 1, both admitted because no session had exec'd yet).
// Constants: MAX_CHANNELS_PER_CONN dialed to 1 (the §2e GW-6/GW-7 override
// row), structural slots stay 2. Baseline:
// gwCalibF4AsBuiltChanCap1.baselineStep (as-built open honors the bound at
// the same cap; HOLDS).
// =========================================================================
module gwCalibF4BoundOnSessionsMap {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 1,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX channel open: the bound guard reads the exec'd-sessions count
  /// (liveSess) instead of the accepted-open count, so opens with no exec
  /// yet always pass it.
  action calibOpenCheckedAgainstSessions(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    chanAt(c, i).ch == ChNone,
    c == ADV or i < B_CHANNEL_SLOTS,
    conn(c).liveSess < MAX_CHANNELS_PER_CONN,
    conns' = conns.set(c, { ...conn(c), stage: CChanOpen, openCh: conn(c).openCh + 1 }),
    chans' = chans.set((c, i), { ...chanAt(c, i), ch: ChAccepted, writeHalf: true }),
    srv' = srv, viol' = viol,
    reach' = { ...reach, burstHitBound: reach.burstHitBound or conn(c).openCh + 1 == MAX_CHANNELS_PER_CONN }
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      calibOpenCheckedAgainstSessions(ADV, i),
      execAdmit(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-2cd23d940 / pre-eef1fbd8d (GW-7): an over-bound or
// non-`session` channel open was refused politely (`Ok(false)`) instead of
// terminating the connection — russh registers channel-table state before
// consulting the handler and never frees it for a refused open, so the
// hidden residue grows without bound while the connection survives.
// PREDICTED: s14RusshResidueBounded falsifies once the per-connection
// residue exceeds the policy bound f = 1 (two politely-refused opens; the
// §2e residue ceiling of 2 keeps the override's violating state inside the
// structural bounds).
// Constants: MAX_CHANNELS_PER_CONN dialed to 1 (so over-bound opens are
// reachable after one accepted open), RESIDUE_BOUND = 1 as built.
// Baseline: gwCalibF4AsBuiltChanCap1.baselineStep (as-built over-bound /
// non-session opens terminate, residue stays ≤ 1; HOLDS).
// =========================================================================
module gwCalibF4RefusePolitely {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 1,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX over-bound open: refused politely (Ok(false)) — the russh
  /// residue entry is retained, the connection survives.
  action calibOverBoundOpenRefused(c: str): bool = all {
    c == ADV,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    conn(c).openCh >= MAX_CHANNELS_PER_CONN,
    conn(c).residue < RESIDUE_CEIL,
    conns' = conns.set(c, { ...conn(c), residue: conn(c).residue + 1 }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// PRE-FIX non-session open (direct-tcpip / x11 / ...): refused politely
  /// via the russh default — state registered, never freed, not counted.
  action calibNonSessionOpenRefused(c: str): bool = all {
    c == ADV,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).residue < RESIDUE_CEIL,
    conns' = conns.set(c, { ...conn(c), residue: conn(c).residue + 1 }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      peerOpenChannel(ADV, i),
      calibOverBoundOpenRefused(c),
      calibNonSessionOpenRefused(c),
      execAdmit(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINES for this file: standard constants (GW-5) and the
// dialed MAX_CHANNELS_PER_CONN = 1 constants (GW-6 / GW-7).
// =========================================================================
module gwCalibF4AsBuilt {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = true,
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
      peerForgedClose(c),
      execAdmit(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter)
    }
  }
}

module gwCalibF4AsBuiltChanCap1 {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 1,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = true,
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
      peerOpenChannel(ADV, i),
      peerOpenOverBound(c),
      peerOpenNonSession(c),
      execAdmit(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i),
      reapTransportEnd(c, letter)
    }
  }
}
