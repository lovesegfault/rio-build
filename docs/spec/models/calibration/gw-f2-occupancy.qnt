// -*- mode: Bluespec; -*-
//
// Stage-C calibration overrides, gateway lifecycle family F2
// (progress-bounded occupancy): every connection stage that is not an
// active protocol session is bounded by a deadline whose expiry frees the
// slot, and answering keepalives must not extend it (gw-session corpus §3
// F2, design §3.4 GW-3; verdicts in the calibration table of
// docs/spec/models/gw-session-invariant-map.md). Same extend-and-override
// pattern as gw-f1-capacity.qnt.
//
// Candidates carried by this file:
//   - GW-3 (79912eda0, repr.)  gwCalibF2OpenDisarmsGrace    -> s20 / l1
//   - GW-3 (a207ee15c const.)  gwCalibF2NoHandshakeDeadline -> l3
//
// Family disposition of the GW-3 constituents NOT carried by their own
// override: 0101be9c6 (grace armed only in channel_close — the
// never-opens population unbounded) is the same wrong-emptiness-signal
// mechanism as the representative at model resolution (the grace fails to
// be armed for an idle authenticated connection), so the
// gwCalibF2OpenDisarmsGrace falsification covers it; aec2521a3 (no
// pre-banner bound) reverts the accept-anchored pre-auth deadline, whose
// loss is the same L1 armed-path falsification one stage earlier — covered
// by the same module's L1 run (the pre-banner stage's only designed path
// is the pre-auth deadline; keepalive cannot reap pre-banner by §2d).
//
// Run (serially, from the repo root):
//   quint verify --backend=tlc --main=<module> --step=calibStep \
//     --invariant=<predicted invariant> \
//     docs/spec/models/calibration/gw-f2-occupancy.qnt

// =========================================================================
// CALIBRATION — pre-79912eda0 (GW-3 representative): connection emptiness
// was measured on OPEN CHANNELS, not exec'd sessions — a channel open
// disarmed the empty-connection grace (and arming happened only on close).
// A client that authenticates, opens a channel and never execs (while
// answering keepalives, which reset every russh timer) holds the slot and
// fd forever with no deadline armed.
// PREDICTED: s20GraceArmedExactlyWhenEmpty falsifies at the first
// channel-open-disarms state (authenticated, zero live sessions, grace not
// armed); l1ConnReclaimArmed falsifies in the same state (no stage
// deadline, no grace, no budget, no designed reap enabled against a
// keepalive-answering peer).
// Constants: famPreauth/famAdmission scope (LET_OCC). Baseline:
// gwCalibF2AsBuilt.baselineStep (as-built peerOpenChannel keeps the grace
// armed; HOLDS).
// =========================================================================
module gwCalibF2OpenDisarmsGrace {
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

  /// PRE-FIX channel open: the open itself counts as "activity" — it
  /// disarms the empty-connection grace (and nothing re-arms it until a
  /// close), so an open-without-exec connection sits with no deadline.
  action calibOpenChannelDisarmsGrace(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    chanAt(c, i).ch == ChNone,
    c == ADV or i < B_CHANNEL_SLOTS,
    conn(c).openCh < MAX_CHANNELS_PER_CONN,
    conns' = conns.set(c, { ...conn(c),
      stage: CChanOpen,
      openCh: conn(c).openCh + 1,
      grace: if (conn(c).grace == GArmed or conn(c).grace == GChecked) GUnarmed else conn(c).grace
    }),
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
      peerWithholdsProgress(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      calibOpenChannelDisarmsGrace(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// CALIBRATION — pre-a207ee15c (GW-3 constituent): the protocol handshake
// phase had no deadline (the opcode idle timeout starts only
// post-handshake). An authenticated client that execs and then sends zero
// bytes parks the session pre-handshake forever.
// PREDICTED: l3PreSessionOccupancyArmed falsifies at the first admission
// (a session occupied pre-handshake with no handshake deadline armed).
// Constants: famAdmission scope. Baseline: gwCalibF2AsBuilt.baselineStep
// (as-built execAdmit arms the handshake deadline; HOLDS).
// =========================================================================
module gwCalibF2NoHandshakeDeadline {
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

  /// PRE-FIX exec admission: the session starts its pre-handshake read
  /// with NO handshake deadline armed (the 30 s bound a207ee15c added).
  action calibExecAdmitNoHandshakeDeadline(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChAccepted,
    chanAt(c, i).writeHalf,
    not(sessAt(c, i).exists),
    srv.sessPermits < MAX_SESSIONS,
    val wasArmed = conn(c).grace == GArmed
    all {
      conns' = conns.set(c, { ...conn(c),
        liveSess: conn(c).liveSess + 1,
        everSess: true,
        grace: if (wasArmed) GUnarmed else conn(c).grace
      }),
      chans' = chans.set((c, i), { ...chanAt(c, i),
        writeHalf: false,
        execs: chanAt(c, i).execs + 1,
        sess: { ...admittedSess, hsArmed: false }
      }),
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
      peerWithholdsProgress(c),
      graceCheckPass(c),
      graceFire(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      calibExecAdmitNoHandshakeDeadline(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

// =========================================================================
// AS-BUILT BASELINE for this file (union alphabet, same constants).
// =========================================================================
module gwCalibF2AsBuilt {
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
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}
