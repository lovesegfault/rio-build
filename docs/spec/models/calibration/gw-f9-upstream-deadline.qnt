// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, gateway lifecycle family F9 (anti-hang
// deadlines on upstream waits): a protocol session never waits unboundedly
// on the store or scheduler — every unary/store RPC await inside a session
// carries a deadline (gw-session corpus §3 F9, design §3.4 GW-16; verdicts
// in the calibration table of docs/spec/models/gw-session-invariant-map.md).
// Same extend-and-override pattern as gw-f1-capacity.qnt. The calibration
// target is the rpc-wait deadline 755f49744 added; the build-event wait is
// deliberately deadline-free (design §2a) and is NOT part of this family's
// claim.
//
// Candidate carried by this file:
//   - GW-16 (755f49744, +0f5408e11-C3, 5bc482e52)
//     gwCalibF9RpcWaitNoDeadline -> l6
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=gwCalibF9RpcWaitNoDeadline \
//     --step=calibStep --invariant=l6RpcWaitDeadlineArmed \
//     docs/spec/models/calibration/gw-f9-upstream-deadline.qnt

// =========================================================================
// CALIBRATION — pre-755f49744 (GW-16): store/scheduler awaits inside the
// session handlers carried no deadline — a stalled upstream parked the
// session (and its channel and buffers) indefinitely.
// PREDICTED: l6RpcWaitDeadlineArmed falsifies at the first un-deadlined
// rpc-wait entry (a session in in-opcode-rpc-wait with no upstream
// deadline armed).
// Constants: famUpstream scope. Baseline: gwCalibF9AsBuilt.baselineStep
// (as-built opcodeToRpcWait arms the deadline; HOLDS).
// =========================================================================
module gwCalibF9RpcWaitNoDeadline {
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

  /// PRE-FIX upstream RPC issue: the await is not with_timeout-wrapped —
  /// the session enters rpc-wait with no deadline armed.
  action calibOpcodeToRpcWaitNoDeadline(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpRead,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpRpc } }),
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
      acceptErrorTransient,
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      calibOpcodeToRpcWaitNoDeadline(ADV, i),
      opcodeToEmit(ADV, i),
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
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINE for this file (famUpstream-shaped alphabet, same
// constants).
// =========================================================================
module gwCalibF9AsBuilt {
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
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      opcodeToEmit(ADV, i),
      fireRpcDeadline(ADV, i),
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
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}
