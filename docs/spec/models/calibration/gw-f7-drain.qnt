// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F7
// (drain/shutdown ordering): shutdown is staged — NOT_SERVING observable,
// stop accepting, wait the session-drain window, cancel stragglers, then
// exit — and established sessions are not killed before their window
// (gw-session corpus §3 F7, design §3.4 GW-13; verdicts in the calibration
// table of docs/spec/models/gw-session-invariant-map.md). Same
// extend-and-override pattern as gw-f1-capacity.qnt. The T half
// re-introduces the over-tight pre-fix behavior (no drain window at all)
// per design §5.
//
// Candidate carried by this file (three arms of GW-13):
//   - 765671437  gwCalibF7DrainExpiryNoCancel       (V) -> l4
//   - d653222cf / a6a5a77b9  gwCalibF7SigtermSkipsStages (V) -> s17
//   - d653222cf  gwCalibF7AcceptStopKillsEstablished (T) -> p6
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f7-drain.qnt

// =========================================================================
// CALIBRATION — pre-765671437 (GW-13, expiry-cancel arm): the shutdown
// token was only selected at the opcode-read point — a session parked in
// the build-event wait never observed it, so drain expiry never cancelled
// in-flight builds and the process exit (the CANCEL_GRACE hard exit)
// happened with the build still running, never cancelled.
// PREDICTED: l4DrainObligationsArmed falsifies at the first hard exit with
// a straggler proto task (process exited, live proto tasks / counted
// connections remain).
// Constants: famDrain scope. Baseline: gwCalibF7AsBuilt.baselineStep
// (as-built token observation reaches build-event-wait and exit requires
// quiescence; HOLDS).
// =========================================================================
module gwCalibF7DrainExpiryNoCancel {
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
    LET_DRAIN = true,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX shutdown-token observation: only sessions at the opcode-read
  /// points (ready / reading client data / pre-handshake) see the token; a
  /// session parked on the build-event stream (or any in-opcode upstream
  /// wait) never does.
  action calibObserveShutdownOpcodeReadOnly(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto != PExited, sessAt(c, i).proto != PCancelling,
    sessAt(c, i).proto == PReady or sessAt(c, i).proto == POpRead or sessAt(c, i).proto == PHandshaking,
    or { srv.phase == SrvTokenFired, chanAt(c, i).ch == ChClosed },
    val s = sessAt(c, i)
    val reason = if (s.proto == PHandshaking) RPreHandshake else RShutdown
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(s, reason) }),
      conns' = conns, srv' = srv, viol' = viol,
      reach' = { ...reach, drainCancelFired: reach.drainCancelFired or (s.tracked and srv.phase == SrvTokenFired) }
    }
  }

  /// PRE-FIX process exit: the CANCEL_GRACE hard exit — once the drain
  /// window has expired the process exits regardless of stragglers. The
  /// guard pins the counterexample to the corpus shape (an in-flight build
  /// still tracked, its CancelBuild never attempted, because nothing woke
  /// the parked handler); without the pin the same override falsifies L4
  /// on shallower exit-with-idle-connection traces that belong to the
  /// stage-collapse arm below.
  action calibSrvExitWithStragglers: bool = all {
    srv.phase == SrvTokenFired,
    CHKEYS.exists(k => sessOf(k).exists and sessOf(k).tracked and not(sessOf(k).cancelSent)),
    srv' = { ...srv, phase: SrvExited },
    conns' = conns, chans' = chans, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      reapTransportEnd(c, letter),
      opSigterm,
      srvAcceptStop,
      srvSessionDrainTimeout,
      calibSrvExitWithStragglers,
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
      calibObserveShutdownOpcodeReadOnly(ADV, i),
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
// CALIBRATION — pre-d653222cf / pre-a6a5a77b9 (GW-13, stage-collapse arm):
// no drain sequencing existed (or the drain token cascade cancelled serve
// instantly, collapsing the window to zero) — SIGTERM took the process
// straight down, killing clients with builds in flight before NOT_SERVING
// was ever observable.
// PREDICTED: s17DrainStageOrder falsifies at the first collapsed shutdown
// (stages skipped); the same trace exits with live sessions (the L4
// consequence).
// Constants: famDrain scope. Baseline: gwCalibF7AsBuilt.baselineStep
// (as-built three-stage advance; HOLDS).
// =========================================================================
module gwCalibF7SigtermSkipsStages {
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
    LET_DRAIN = true,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX SIGTERM handling: the process exits immediately — no
  /// NOT_SERVING probe window, no accept-stop stage, no session-drain
  /// window, no straggler cancel. The guard pins the counterexample to the
  /// corpus harm (a client with a live session is killed by the rollout).
  action calibSigtermExitsImmediately: bool = all {
    LET_DRAIN,
    srv.phase == SrvServing,
    CONNS.exists(c => conn(c).exists and conn(c).live and conn(c).liveSess > 0),
    srv' = { ...srv, phase: SrvExited },
    viol' = { ...viol, drainOrderViolated: true },
    conns' = conns, chans' = chans, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      reapTransportEnd(c, letter),
      calibSigtermExitsImmediately,
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
      protoObservesShutdown(ADV, i),
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
// CALIBRATION — GW-13 trace-admission (T) half, per design §5: the
// over-tight pre-d653222cf shutdown re-introduced — stopping the accept
// loop tears down established connections and their in-flight sessions
// instead of letting them drain (the rollout that kills clients with
// builds in flight).
// PREDICTED: p6EstablishedSurviveAcceptStop falsifies at the first
// accept-stop that terminates an established connection with a live
// session.
// Constants: famDrain scope. Baseline: gwCalibF7AsBuilt.baselineStep
// (as-built accept-stop touches no established connection; HOLDS).
// =========================================================================
module gwCalibF7AcceptStopKillsEstablished {
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
    LET_DRAIN = true,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).* from "../gwConnLifecycle"

  /// PRE-FIX accept-stop: stopping the listener also tears down an
  /// established connection (and the SessionGuards riding on it) before
  /// the drain window.
  action calibAcceptStopKillsEstablished: bool = all {
    srv.phase == SrvNotServing,
    val victims = CONNS.filter(c => conn(c).exists and conn(c).live and conn(c).liveSess > 0)
    all {
      victims.size() > 0,
      val released = victims.fold(0, (acc, c) => acc + connGuards(c))
      val permitsHeld = victims.filter(c => conn(c).permit).size()
      val countedHeld = victims.filter(c => conn(c).counted).size()
      all {
        srv' = { ...srv,
          phase: SrvAcceptStopped,
          connPermits: srv.connPermits - permitsHeld,
          connsActive: srv.connsActive - countedHeld,
          sessPermits: srv.sessPermits - released,
          chansActive: srv.chansActive - released
        },
        conns' = CONNS.mapBy(c => if (victims.contains(c)) connOnDrop(conn(c)) else conn(c)),
        chans' = CHKEYS.mapBy(k => if (victims.contains(k._1)) chanOnConnDrop(chans.get(k)) else chans.get(k)),
        viol' = { ...viol, acceptStopKilled: true },
        reach' = reach
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
      reapTransportEnd(c, letter),
      opSigterm,
      calibAcceptStopKillsEstablished,
      srvSessionDrainTimeout,
      srvExit,
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
      protoObservesShutdown(ADV, i),
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
// AS-BUILT BASELINE for this file (famDrain alphabet, same constants —
// the wired quint-gw-lifecycle-fam-drain check is the same proof; this
// module re-runs it with the file's exact alphabet so the baseline is
// attributable run-for-run).
// =========================================================================
module gwCalibF7AsBuilt {
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
    LET_DRAIN = true,
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
      reapTransportEnd(c, letter),
      opSigterm,
      srvAcceptStop,
      srvSessionDrainTimeout,
      srvExit,
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
      protoObservesShutdown(ADV, i),
      protoObservesConnGone(ADV, i),
      cancelLoopDone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}
