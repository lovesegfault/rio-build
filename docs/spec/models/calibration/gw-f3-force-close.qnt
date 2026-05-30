// -*- mode: Bluespec; -*-
//
// Stage-C calibration override, gateway lifecycle family F3
// (decision-to-enforcement): once the gateway decides to disconnect, a
// kill deadline is armed at the decision point and enforced regardless of
// peer cooperation; nothing un-decides it (gw-session corpus §3 F3, design
// §3.4 GW-4; verdicts in the calibration table of
// docs/spec/models/gw-session-invariant-map.md). Same extend-and-override
// pattern as gw-f1-capacity.qnt.
//
// Candidate carried by this file:
//   - GW-4 (1c46d9781, repr.)  gwCalibF3DecideWithoutArm -> s12
//
// Family disposition of the GW-4 constituents NOT carried by their own
// override:
//   - 2f11bb8f5 (enforcement only on the read path; TCP_USER_TIMEOUT
//     backstop added): at the model's armed-style resolution this revert is
//     the loss of the parked-write enforcement wake (`parkWake`) plus the
//     kernel reap letter — exactly the state the wired
//     quint-gw-lifecycle-falsification-l1-no-inactivity probe exhibits
//     (L1-strict falls in fault-degraded when both are absent and only the
//     inactivity backstop remains). The probe's counterexample trace is the
//     pre-fix bug class at model resolution; a separate override would
//     re-derive it. Recorded as a structural-argument disposition in the
//     calibration table, not silently dropped.
//   - 5a50b3f70 (KEX-parked pre-auth connections outlive the grace because
//     the polite disconnect is undeliverable): the enforcement half is the
//     same decide-implies-arm + transport-deadline mechanism this module's
//     representative reverts (S12), and the occupancy half is GW-3's
//     surface; the famPreauth exhaustive check carries the as-built
//     KEX-parked reclamation (canReachKexParkedReclaimed).
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=gwCalibF3DecideWithoutArm \
//     --step=calibStep --invariant=s12DecideImpliesArmed \
//     docs/spec/models/calibration/gw-f3-force-close.qnt

// =========================================================================
// CALIBRATION — pre-1c46d9781 (GW-4): the auth-timeout path queued the
// polite SSH_MSG_DISCONNECT WITHOUT arming the transport force-close, on
// the assumption the pre-auth read deadline still covered the connection —
// but that deadline stops applying the instant authentication completes,
// so an auth that straddles the timeout escapes both bounds and a peer
// that ignores the disconnect is bounded only by russh leftovers.
// PREDICTED: s12DecideImpliesArmed falsifies at the first pre-fix
// disconnect-queued state (discQ with no force-close armed).
// Constants: famPreauth scope (LET_OCC). Baseline:
// gwCalibF3AsBuilt.baselineStep (the as-built firePreAuthPhase2 arms at the
// decision point; HOLDS).
// =========================================================================
module gwCalibF3DecideWithoutArm {
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

  /// PRE-FIX pre-auth deadline phase 2: the polite disconnect is queued
  /// but the ForceClose is NOT armed at the decision point.
  action calibPreAuthPhase2NoArm(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).preAuth == PaArmed,
    conn(c).banner, conn(c).stage != CAuthed, conn(c).stage != CChanOpen,
    not(compliant(c)), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), preAuth: PaFired, discQ: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  action calibStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthFail(c),
      peerAuthOk(c),
      peerSilent(c),
      peerParkInKex(c),
      peerCompletesKex(c),
      peerWithholdsProgress(c),
      firePreAuthPhase1(c),
      calibPreAuthPhase2NoArm(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINE for this file (famPreauth alphabet, same constants).
// =========================================================================
module gwCalibF3AsBuilt {
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
    nondet letter = REAP_LETTERS.oneOf()
    any {
      acceptConn(c),
      peerBanner(c),
      peerAuthFail(c),
      peerAuthOk(c),
      peerSilent(c),
      peerParkInKex(c),
      peerCompletesKex(c),
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
