// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, gateway lifecycle family F10 (connection
// accept-path availability): a transient accept(2) failure must not
// terminate the accept loop or the process; only shutdown ends the
// listener (gw-session corpus §3 F10, design §3.4 GW-19; verdicts in
// the VERDICT block at the end of this header). Same
// extend-and-override pattern as gw-f1-capacity.qnt.
//
// Candidate carried by this file:
//   - GW-19 (9b693441f)  gwCalibF10AcceptErrorFatal -> s18
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=gwCalibF10AcceptErrorFatal \
//     --step=calibStep --invariant=s18ListenerSurvivesAcceptErrors \
//     docs/spec/models/calibration/gw-f10-accept.qnt

// =========================================================================
// CALIBRATION — pre-9b693441f (GW-19): a transient accept(2) error
// (ECONNABORTED, EMFILE under load) propagated out of run(); main exited
// past the session drain and every live session was aborted with it. The
// pre-fix letter below terminates the listener and the process at the
// error instead of sleeping and retrying.
// PREDICTED: s18ListenerSurvivesAcceptErrors falsifies at the first fatal
// accept error (the listener-death latch); on the deeper traces the same
// module exits the process with established connections and live proto
// tasks still up (the L4 consequence the corpus describes).
// Constants: famUpstream-style LET_UPSTREAM (the EMFILE letter) plus the
// minimal session bring-up so the "all live sessions aborted" consequence
// is visible in the explored space. Baseline:
// gwCalibF10AsBuilt.baselineStep (as-built acceptErrorTransient is a
// retry; HOLDS).
// =========================================================================
//
// VERDICT (Stage-C calibration, relocated from the retired gw-session
// invariant map; scope/baseline protocol and the acceptance table:
// calibration/README.md, gateway section; trace walks verbatim from the
// Stage-C record):
// - GW-19 gwCalibF10AcceptErrorFatal: FALSIFIES S18 @ 3 (82/24) — a
//   transient accept error treated as fatal falls via the listener-death
//   latch. Note: S18's content is latch-shaped (the as-built letter is a
//   no-op by construction), so the override is necessarily a latch-setter;
//   the pinned counterexample exits the process with a live connection up,
//   making the structural consequence (the corpus's "all live sessions
//   aborted", the L4 form) visible in the same trace (9b693441f).
// - BASELINE: gwCalibF10AsBuilt (GW-19) 49,315 distinct / 44 s — HOLDS
//   allInvariants.
module gwCalibF10AcceptErrorFatal {
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

  /// PRE-FIX transient accept error: treated as fatal — the accept loop
  /// returns the error, run() propagates it, and the process exits
  /// without any drain staging. The guard pins the counterexample to the
  /// corpus harm (the EMFILE storm arrives while connections are up, so
  /// the same exit state also shows the live sessions it aborts).
  action calibAcceptErrorFatal: bool = all {
    LET_UPSTREAM,
    or { srv.phase == SrvServing, srv.phase == SrvNotServing },
    CONNS.exists(c => conn(c).exists and conn(c).live),
    srv' = { ...srv, phase: SrvExited },
    viol' = { ...viol, listenerDied: true },
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
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      calibAcceptErrorFatal,
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
// AS-BUILT BASELINE for this file (same alphabet with the as-built
// retry-and-continue accept-error letter, same constants).
// =========================================================================
module gwCalibF10AsBuilt {
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
      acceptErrorTransient,
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
