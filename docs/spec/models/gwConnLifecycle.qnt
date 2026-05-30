// -*- mode: Bluespec; -*-
//
// Phase-0 Stage-B model of the rio-gateway connection/channel/session/server
// lifecycle (campaign gw-session-formal, design gw-session-formal-design.md
// DRAFT v2 §2-§3). One transport (the SSH listener), sessions as compound
// nodes, the data plane abstracted to occupancy states. The invariant ↔
// spec-rule linkage lives in docs/spec/models/gw-session-invariant-map.md;
// check wiring and r[verify] markers live in nix/quint.nix, never here.
//
// WHAT IS MODELED (design §2a/§2b/§2c)
//   - Two connection slots with fixed asymmetric roles: "cA" plays the
//     adversary letters (the regime constants gate which families), "cB"
//     plays legitimate letters only. Per connection: the ConnStage machine
//     (TcpAccepted → AuthAttempted → Authenticated → ChannelOpen,
//     server/connection.rs ConnStage), the conn permit acquired at accept
//     (server/mod.rs new_client try_acquire), the connections_active gauge
//     latched at the first auth callback (mark_real_connection), the
//     pre-auth deadline, the two-action empty-connection grace timer body
//     (arm_if_idle / the timer task's re-check then disconnect), ForceClose
//     arming (armed-once, never disarmed) and ConnDeadline expiry, and the
//     handler-error teardown path.
//   - The russh transport environment (§2b, eight explicit variables):
//     handle queue (depth abstracted to 2), per-channel window credits,
//     per-channel non-window pendingData (always 0 as-built; calibration
//     overrides use it), refused-open residue, KEX-active, write-parked,
//     disconnect-queued, transport-live + the reap letters. The
//     keepalive-reset-by-any-packet rule and the reap-letter enabling
//     conditions are encoded as transition-relation guards (§2d table).
//   - Per connection × 2 channel slots: accepted/closed bookkeeping
//     (note_channel_accepted/note_channel_closed), the write half consumed
//     by exec admission, channel_eof half-close.
//   - Per channel ≤1 protocol session, modeled as a compound node:
//     proto-phase (handshaking / ready / in-opcode-client-read /
//     in-opcode-rpc-wait / in-opcode-build-event-wait / in-opcode-emitting /
//     cancelling / exited(reason class)), egress/finish-phase (forwarding /
//     send-pending / close-out / done / aborted), the SessionGuard bit, the
//     client-pump-live bit, ≤1 tracked build with the wire/non-wire/terminal
//     removal policy (handler/build.rs active_build_ids), the per-build
//     upstream-stream-held bit, the close-out ordering bits
//     (exit-status ≺ eof ≺ close, finish_channel_session), and the
//     per-scope timer-armed bits (handshake, opcode-idle, send budget,
//     close-out budget, rpc-wait deadline).
//   - The server level: accept loop, transient-accept-error survival, and
//     the three-stage drain (NOT_SERVING → accept-stop → sessions_shutdown
//     → exit, main.rs / r[gw.drain.three-stage]).
//   - The operator alphabet is exactly {SIGTERM} (closed world: no admin
//     kill/session-list RPC exists; key revocation affects future auth
//     only — gateway.typ operations note).
//
// WHAT IS NOT MODELED (design §2a loss table; dispositions in the map)
//   - The in-opcode data plane (upload framing, NAR bytes, per-entry
//     bounds, drv_cache budgets): collapsed to the rpc-wait /
//     build-event-wait / emitting occupancy states. GW-14/15/21 are
//     out-of-model (covered by wire/golden/unit tests).
//   - build-event-wait has no deadline exit (faithful: the
//     process_build_events select has no timeout arm). "Scheduler accepts
//     then stays silent forever with a live patient client" is an
//     environment assumption, not a verified bound.
//   - No credential clock (JWT TTL, GW-17), no log-tail/spawn_blocking
//     sub-tasks (folded into "session-owned resources released"), no
//     client-pump-vs-reap two-step (W6), no WatchBuild reconnect loop
//     internals (its boundedness is an imported assumption).
//   - The W7 residual sliver (force-close armed only after the two
//     already-registered russh wakes are spent) is below this abstraction:
//     the model tracks one bit of park ordering (was the force-close armed
//     when the write parked) and otherwise treats enforcement as available
//     once armed.
//   - SIGKILL, the scheduler/store side of cancel, and the orphan-watcher
//     backstop are environment assumptions named in the invariant map.
//
// SERIALIZATION GROUND RULES (design §2d, model header obligations)
//   1. Each russh Handler callback is one atomic action; callbacks of one
//      connection never interleave with each other (single ssh-session
//      task). 2. exec_request's gate sequence (cap check → write-half take
//      → session_admitted → channel_success → gauge → guard creation →
//      task spawns) is one atomic action; the only splitter is the
//      panic fault letter (fault-degraded regime). 3. The empty-grace
//      timer body is two actions (check-pass, then disconnect+arm) so the
//      documented admit-vs-fire race (W3/P4) is representable. 4. Session
//      proto/egress steps, timer firings, force-close enforcement, accept
//      and drain advances interleave freely. 5. No temporal operators, no
//      checker-level fairness: settlement claims are armed-style state
//      invariants; reclamation traces are pinned by witnesses and named
//      runs (Stage C wiring).
//   rsNoAbortHandle: the gateway holds no abort handle on the russh session
//      loop — only I/O failure, peer close, deadline expiry, keepalive /
//      inactivity / TCP_USER_TIMEOUT exhaustion, or a handler error end it.
//   rsCallbackSerial: all Handler callbacks of one connection dispatch
//      serially from the single ssh-session task.
//
// ENCODING DECISIONS (recorded in the invariant map Stage-B record)
//   - Bookkeeping counters (connections_active, channels_active, per-conn
//     open_channels / live_sessions, permit counts) are explicit state
//     variables mutated by the actions, not definitions derived from sets,
//     so the divergence calibrations have something to skew (§2e).
//   - Peer classes are sticky per-connection latches set by the played
//     letters (occupancy-withholding, transport-withholding); every
//     budget/grace/reap fire action carries the §2d peer-class + stage
//     enabling condition as a guard.
//   - The TCP_USER_TIMEOUT reap is keyed on the transport-withholding /
//     vanished classes (never the compliant class) rather than on a
//     literal ACK distinction; the park-ordering bit (parkWake) makes the
//     W7 degraded tier expressible. Both deviations are §2 under-
//     specification calls recorded in the invariant map.
//   - Process exit is modeled as enabled only once every connection and
//     proto task is gone (the CANCEL_GRACE hard-exit with stragglers is
//     wall-clock behavior below the tick-free abstraction).
//   - Channel slots are use-once (a closed slot is not reused); the
//     over-bound / non-session open attempt letters need no free slot, so
//     they stay enabled at the structural ceiling (§2e).
//   - Data egress pacing (window credits) and the handle queue are
//     decoupled: window credits pace session data sends (GW-8 surface),
//     the handle queue carries close-out/disconnect deliveries and the
//     write-park mechanics (GW-2/W7 surface).
//
// The invariants, witnesses, regimes and their check wiring are documented
// at their definitions and in nix/quint.nix. Measured state counts, depths
// and wall-clocks live in the introducing commits' messages and the CI
// checks' transcripts, never in this file.
module gwConnLifecycle {

  // --- 1. types -----------------------------------------------------------

  /// ConnStage (rio-gateway/src/server/connection.rs, AtomicU8, monotone).
  type CStage = CTcp | CAuthAtt | CAuthed | CChanOpen

  /// Pre-auth deadline state: armed at accept, latched off at auth success
  /// (ConnDeadline.authenticated), fired = the phase-1/phase-2 deadline ran.
  type PreAuthSt = PaArmed | PaLatched | PaFired

  /// Empty-connection grace (EmptyConnectionTimer): unarmed / armed /
  /// re-check passed (the two-action body, connection.rs timer task) /
  /// fired (disconnect queued + force-close armed).
  type GraceSt = GUnarmed | GArmed | GChecked | GFired

  /// SSH channel slot state (accepted_channels / note_channel_closed).
  type ChSt = ChNone | ChAccepted | ChClosed

  /// Proto-task phase of the compound session node (design §2a):
  /// run_protocol_loop's occupancy states, with the in-opcode upstream
  /// occupancy split into rpc-wait (deadline-carrying) and
  /// build-event-wait (deliberately deadline-free).
  type PPhase =
    | PHandshaking
    | PReady
    | POpRead
    | POpRpc
    | POpBuild
    | POpEmit
    | PCancelling
    | PExited

  /// Exit-reason classes (the P1-P12 exit paths collapsed, design §2a).
  type ExitR =
    | RNone
    | RPreHandshake
    | RHandshakeFail
    | RIdle
    | RClientEof
    | RWire
    | RShutdown
    | RHandlerErr
    | RPanic

  /// Egress/finish phase of the compound session node: the response pump
  /// (pump_session_responses) and finish_channel_session collapsed to the
  /// release-then-close-out ladder; EgAborted is the ChannelSession::Drop
  /// abort path (channel close / connection teardown).
  type EgPhase = EgFwd | EgSendPend | EgCloseOut | EgDone | EgAborted

  /// Server/process phase (main.rs three-stage drain).
  type SrvPhase = SrvServing | SrvNotServing | SrvAcceptStopped | SrvTokenFired | SrvExited

  /// Transport-reap letters (§2b rsTransportLive: the only ways the russh
  /// session loop returns and the handler drops).
  type ReapLetter = LPeerClose | LHandlerError | LDeadlineExpired | LKeepalive | LTcpUserTimeout | LInactivity

  /// Per-session compound node (design §2a). exists=false means the slot's
  /// channel has not admitted an exec.
  type SessRec = {
    exists: bool,
    proto: PPhase,
    exitR: ExitR,
    eg: EgPhase,
    guard: bool,        // SessionGuard held (permit + gauge + live-count)
    pump: bool,         // client pump task live
    hsArmed: bool,      // handshake timeout armed
    idleArmed: bool,    // opcode idle timeout armed
    sendBudget: bool,   // HANDLE_SEND_TIMEOUT armed for a pending send
    coBudget: bool,     // close-out budget armed
    rpcDl: bool,        // upstream rpc-wait deadline armed (L6)
    tracked: bool,      // active_build_ids non-empty (≤1 build)
    everTracked: bool,
    terminal: bool,     // a terminal build outcome was processed
    nonWireErr: bool,   // the build stream failed with a non-Wire class
    streamHeld: bool,   // the per-build BuildEvent stream is held (L7/W10)
    cancelSent: bool,   // a CancelBuild attempt was made (cancel_active_builds)
    coStatus: bool,     // close-out: exit-status sent
    coEof: bool,        // close-out: eof sent
    coClose: bool       // close-out: close sent
  }

  /// Per-channel slot record; the session compound node is embedded.
  type ChanRec = {
    ch: ChSt,
    writeHalf: bool,    // ChannelWriteHalf still in channel_writers
    execs: int,         // admitted execs on this channel (S6: ≤ 1)
    clientEof: bool,    // channel_eof seen (client_tx taken)
    window: int,        // client-granted SSH window credits (rsWindow)
    pending: int,       // russh unbounded buffer occupancy (rsPendingData; 0 as-built)
    sess: SessRec
  }

  /// Per-connection record: ConnStage + implicit flag states + the russh
  /// environment component (§2b) + peer-class tracking (§2d) + explicit
  /// bookkeeping counters (§2e).
  type ConnRec = {
    exists: bool,
    stage: CStage,
    permit: bool,       // conn-cap permit held (None = over cap, torn down at first auth callback)
    counted: bool,      // active_conns / connections_active incremented (auth_attempted latch)
    live: bool,         // russh session loop alive (rsTransportLive)
    herr: bool,         // handler returned an error; the loop will end (reap letter LHandlerError)
    banner: bool,       // SSH transport established (version exchange done)
    kex: bool,          // KEX in flight (rsKexActive): queue not drained, disconnect undeliverable
    parked: bool,       // session loop parked in an inline socket write (rsWriteParked)
    parkWake: bool,     // the force-close was already armed when the park began (enforcement wake registered)
    discQ: bool,        // a polite SSH_MSG_DISCONNECT is queued (rsDisconnectQueued)
    queue: int,         // handle/event queue occupancy (rsHandleQueue, depth abstracted)
    residue: int,       // refused/non-session open channel-table residue (rsResidue)
    preAuth: PreAuthSt,
    grace: GraceSt,
    fcArmed: bool,      // ForceClose armed (earliest-deadline-wins, never disarmed)
    fcByBudget: bool,   // armed by a stall/close-out budget site (P5 bookkeeping)
    expired: bool,      // ConnDeadline expired: no further bytes accepted or delivered
    occW: bool,         // peer played an occupancy-withholding letter (§2d class)
    trW: bool,          // peer played a transport-withholding letter (§2d class)
    reading: bool,      // peer drains our writes (false after stop-draining)
    sendsPk: bool,      // peer keeps sending SSH packets (keepalive/inactivity reset rule)
    acks: bool,         // peer ACKs at TCP level (false after vanish-half-open)
    tcpUtoOk: bool,     // TCP_USER_TIMEOUT setsockopt succeeded at accept
    openCh: int,        // per-connection open_channels counter (explicit, S7)
    liveSess: int,      // per-connection live_sessions counter (explicit, S4)
    panicLeaks: int,    // live_sessions increments leaked by the exec-gate panic splitter
    everSess: bool,     // ≥1 exec was admitted on this connection
    sawZero: bool       // live_sessions touched zero after having been positive
  }

  /// Server/global record: process phase + the global gauges/permits as
  /// explicit counters (§2e).
  type SrvRec = {
    phase: SrvPhase,
    connsActive: int,   // connections_active gauge == active_conns drain signal
    chansActive: int,   // channels_active gauge (live SessionGuards)
    connPermits: int,   // conn_sem permits held
    sessPermits: int    // session_sem permits held
  }

  /// Cross-action "never happens" latches. Each is set only by behavior the
  /// production guards forbid; Stage-C overrides revert a guard and expect
  /// the matching invariant to falsify.
  type Viol = {
    execOnNonOpenChannel: bool,   // an exec was admitted on a never-accepted/closed channel (S7)
    permitStranded: bool,         // a permit left held with no guard and no admission in progress (S8)
    doubleRelease: bool,          // a permit/gauge slot/live-count released twice (S9)
    fcDisarmed: bool,             // an armed force-close was cleared (S11)
    deliveredAfterExpiry: bool,   // bytes accepted from / delivered to the peer after expiry (S11)
    drainOrderViolated: bool,     // a drain stage advanced out of order (S17)
    acceptStopKilled: bool,       // accept-stop terminated an established connection/session (S17/P6)
    listenerDied: bool,           // a transient accept error stopped the accept loop (S18)
    execRefusedBelowCap: bool,    // a well-formed exec was refused below the global session cap (P1)
    disconnectOnTouchZero: bool,  // a connection was disconnected for touching zero sessions before the grace fired (P2)
    capRefusedOpenOrConn: bool,   // capacity exhaustion refused a channel open or killed a connection (P3)
    gracePreCheckAdmitKilled: bool, // a grace cycle disconnected a connection that admitted before the re-check (P4)
    budgetFiredOnCompliant: bool, // a send/close-out budget fired against a compliant-class peer (P5)
    timerReapedCompliant: bool,   // a keepalive/TCP_USER_TIMEOUT/inactivity reap fired against a compliant-class peer (P5)
    graceKilledLiveSession: bool  // the grace disconnect ran with ≥1 live SessionGuard outside the documented race (L5)
  }

  /// Reachability latches feeding the expect-violation witnesses (§5).
  type Reach = {
    stallFcArmed: bool,           // a stalled send armed the force-close
    releaseWithChannelOpen: bool, // a server-side ending released capacity with the channel still open
    graceFired: bool,             // the grace fired on an idle authenticated connection
    execDisarmedGrace: bool,      // an exec admitted while the grace was armed disarmed it
    kexParkedReclaimed: bool,     // a KEX-parked pre-auth connection was reclaimed
    forgedCloseIgnored: bool,     // a forged/duplicate close was ignored
    overBoundTerminated: bool,    // an over-bound open terminated the connection
    burstHitBound: bool,          // a burst of opens reached the per-connection bound
    capExecRejected: bool,        // a session-cap exec rejection occurred (channel open had succeeded)
    overCapAuthTorn: bool,        // an over-cap connection reached its first auth callback permit-less and was torn down
    drainCancelFired: bool,       // a drain-expiry shutdown-token cancel fired on a tracked build
    muxReExec: bool,              // a connection touched zero sessions and exec'd again
    muxSiblingQuiesced: bool,     // a session reached tasks==0 while a sibling session stayed live
    vanishReclaimedDesigned: bool,// a half-open vanish was reclaimed by a designed transport letter
    parkedReclaimedByInactivity: bool, // a parked-write connection was reclaimed via the inactivity letter (W7 degraded tier)
    nonWireRemoved: bool,         // a non-Wire stream error removed a tracked build
    closeOutOrdered: bool,        // a close-out completed in order (exit-status, eof, close)
    graceFiredWithLiveSession: bool // the grace fired in a state with ≥1 live guard (the documented P4/W3 race; L5-strict probe)
  }

  // --- 2. constants (const = overridable per regime / Stage-C override) ----

  /// Policy caps (real values 1000 / 512 / 4096), dialed to the §2e table
  /// values so cap-exceeding triggers are reachable inside the structural
  /// ceilings. Structural ceilings themselves are the fixed slot sets and
  /// the action preconditions below.
  const MAX_CONNECTIONS: int
  const MAX_CHANNELS_PER_CONN: int
  const MAX_SESSIONS: int
  /// Residue policy bound f (as-built terminate-on-refusal keeps residue ≤ 1).
  const RESIDUE_BOUND: int

  /// Regime alphabet switches (§2e regime definitions).
  const LET_OCC: bool        // occupancy-withholding peer letters (silent, KEX-park, never-auth, no-exec, idle)
  const LET_WEDGE: bool      // stop-draining + withhold-window-adjust (the park-producing pair)
  const LET_HOSTILE: bool    // forged close, non-session open, over-bound burst, vanish-half-open, wire garbage
  const LET_UPSTREAM: bool   // scheduler/store slow/unavailable, stream errors, EMFILE
  const LET_DRAIN: bool      // SIGTERM + drain stage advances
  const LET_PANIC: bool      // proto-task panic + exec-gate panic splitter
  const LET_SOCKOPT_FAIL: bool // the TCP_USER_TIMEOUT setsockopt-failed letter
  const INACTIVITY_AVAILABLE: bool // the russh inactivity reap letter exists (fault-degraded only)

  /// Structural ceilings (TLC finiteness; environment bounds, not policy).
  pure val QUEUE_DEPTH = 2
  pure val WINDOW_CREDITS = 2
  pure val RESIDUE_CEIL = 2
  pure val PENDING_CEIL = 2

  /// Connection slots: fixed asymmetric roles (§2e). cA plays the adversary
  /// letters; cB plays legitimate letters only.
  pure val CONNS = Set("cA", "cB")
  pure val ADV = "cA"
  pure val CHANS = Set(0, 1)
  pure val CHKEYS = tuples(CONNS, CHANS)
  pure val REAP_LETTERS = Set(LPeerClose, LHandlerError, LDeadlineExpired, LKeepalive, LTcpUserTimeout, LInactivity)

  // --- 3. pure defs --------------------------------------------------------

  pure def freshSess: SessRec = {
    exists: false,
    proto: PExited,
    exitR: RNone,
    eg: EgDone,
    guard: false,
    pump: false,
    hsArmed: false,
    idleArmed: false,
    sendBudget: false,
    coBudget: false,
    rpcDl: false,
    tracked: false,
    everTracked: false,
    terminal: false,
    nonWireErr: false,
    streamHeld: false,
    cancelSent: false,
    coStatus: false,
    coEof: false,
    coClose: false
  }

  pure def freshChan: ChanRec = {
    ch: ChNone,
    writeHalf: false,
    execs: 0,
    clientEof: false,
    window: WINDOW_CREDITS,
    pending: 0,
    sess: freshSess
  }

  pure def freshConn: ConnRec = {
    exists: false,
    stage: CTcp,
    permit: false,
    counted: false,
    live: false,
    herr: false,
    banner: false,
    kex: false,
    parked: false,
    parkWake: false,
    discQ: false,
    queue: 0,
    residue: 0,
    preAuth: PaArmed,
    grace: GUnarmed,
    fcArmed: false,
    fcByBudget: false,
    expired: false,
    occW: false,
    trW: false,
    reading: true,
    sendsPk: true,
    acks: true,
    tcpUtoOk: true,
    openCh: 0,
    liveSess: 0,
    panicLeaks: 0,
    everSess: false,
    sawZero: false
  }

  pure def noViol: Viol = {
    execOnNonOpenChannel: false,
    permitStranded: false,
    doubleRelease: false,
    fcDisarmed: false,
    deliveredAfterExpiry: false,
    drainOrderViolated: false,
    acceptStopKilled: false,
    listenerDied: false,
    execRefusedBelowCap: false,
    disconnectOnTouchZero: false,
    capRefusedOpenOrConn: false,
    gracePreCheckAdmitKilled: false,
    budgetFiredOnCompliant: false,
    timerReapedCompliant: false,
    graceKilledLiveSession: false
  }

  pure def noReach: Reach = {
    stallFcArmed: false,
    releaseWithChannelOpen: false,
    graceFired: false,
    execDisarmedGrace: false,
    kexParkedReclaimed: false,
    forgedCloseIgnored: false,
    overBoundTerminated: false,
    burstHitBound: false,
    capExecRejected: false,
    overCapAuthTorn: false,
    drainCancelFired: false,
    muxReExec: false,
    muxSiblingQuiesced: false,
    vanishReclaimedDesigned: false,
    parkedReclaimedByInactivity: false,
    nonWireRemoved: false,
    closeOutOrdered: false,
    graceFiredWithLiveSession: false
  }

  /// The proto task takes one of its exit edges: a tracked build routes
  /// through the cancel loop (cancel_active_builds, session.rs) first;
  /// otherwise the task exits directly. Per-scope timers disarm; the
  /// handler-local BuildEvent stream drops with the handler frame.
  pure def protoExit(s: SessRec, reason: ExitR): SessRec = {
    ...s,
    proto: if (s.tracked) PCancelling else PExited,
    exitR: reason,
    hsArmed: false,
    idleArmed: false,
    rpcDl: false,
    streamHeld: false
  }

  /// The session record at exec admission (connection.rs exec_request gate,
  /// one atomic action per ground rule §2d-2).
  pure def admittedSess: SessRec = {
    ...freshSess,
    exists: true,
    proto: PHandshaking,
    exitR: RNone,
    eg: EgFwd,
    guard: true,
    pump: true,
    hsArmed: true
  }

  /// Connection-level bookkeeping when one of its SessionGuards is released
  /// (SessionGuard::Drop → session_ended → arm_if_idle): live_sessions
  /// decrements, the empty grace re-arms when the count touches zero on a
  /// still-live connection with no disconnect queued and no fired grace.
  pure def connAfterGuardRelease(k: ConnRec): ConnRec = {
    val newLive = k.liveSess - 1
    {
      ...k,
      liveSess: newLive,
      sawZero: k.sawZero or newLive == 0,
      grace:
        if (newLive == 0 and k.live and not(k.discQ) and not(k.expired) and not(k.herr) and k.grace == GUnarmed)
          GArmed
        else
          k.grace
    }
  }

  /// Channel record at connection teardown (handler Drop: sessions map
  /// dropped → ChannelSession::Drop aborts the response task; the guard is
  /// released with it; close-outs in flight are abandoned).
  pure def chanOnConnDrop(k: ChanRec): ChanRec = {
    ...k,
    ch: if (k.ch == ChAccepted) ChClosed else k.ch,
    writeHalf: false,
    sess:
      if (not(k.sess.exists)) k.sess
      else {
        ...k.sess,
        guard: false,
        eg: if (k.sess.eg == EgFwd or k.sess.eg == EgSendPend or k.sess.eg == EgCloseOut) EgAborted else k.sess.eg,
        sendBudget: false,
        coBudget: false
      }
  }

  /// Connection record at transport end (russh loop returned → handler
  /// dropped): permit released, parked write gone, counters zeroed. The
  /// counted latch stays (the gauge decrement is the srv-side arithmetic).
  pure def connOnDrop(k: ConnRec): ConnRec = {
    ...k,
    live: false,
    permit: false,
    parked: false,
    kex: false,
    queue: 0,
    openCh: 0,
    liveSess: 0
  }

  // --- 4. state variables ---------------------------------------------------

  var conns: str -> ConnRec
  var chans: (str, int) -> ChanRec
  var srv: SrvRec
  var viol: Viol
  var reach: Reach

  // --- 5. derived state ------------------------------------------------------

  def conn(c: str): ConnRec = conns.get(c)
  def chanAt(c: str, i: int): ChanRec = chans.get((c, i))
  def sessAt(c: str, i: int): SessRec = chans.get((c, i)).sess
  def sessOf(k: (str, int)): SessRec = chans.get(k).sess

  /// Guard-held sessions (the owner-decision definition of "session" for
  /// capacity/autoscaling accounting).
  def heldGuards: int = CHKEYS.filter(k => chans.get(k).sess.guard).size()
  def connGuards(c: str): int = CHANS.filter(i => chans.get((c, i)).sess.guard).size()
  def liveProtos: int = CHKEYS.filter(k => chans.get(k).sess.exists and chans.get(k).sess.proto != PExited).size()

  /// Peer-class predicates (§2d): a connection is compliant while it has
  /// played no withholding letter.
  def compliant(c: str): bool = not(conn(c).occW) and not(conn(c).trW)

  /// Force-close enforcement availability: once armed it can fire on the
  /// next I/O poll; a write that parked before arming has no registered
  /// wake (the W7 ordering bit), so enforcement reaches it only if the park
  /// began with the deadline already armed.
  def fcEnforceable(c: str): bool = conn(c).fcArmed and (not(conn(c).parked) or conn(c).parkWake)

  /// §2d designed transport-reap letter enabling conditions (read-only
  /// forms used by guards and by the L1/L2 armed-path disjunctions).
  def keepaliveEnabled(c: str): bool = and {
    conn(c).live, conn(c).banner, not(conn(c).sendsPk), not(conn(c).parked), not(conn(c).expired)
  }
  def tcpUtoEnabled(c: str): bool = and {
    conn(c).live, conn(c).tcpUtoOk, not(conn(c).expired),
    or {
      not(conn(c).acks),
      and { conn(c).trW, or { conn(c).parked, conn(c).queue > 0, conn(c).discQ } }
    }
  }
  def inactivityEnabled(c: str): bool = and {
    INACTIVITY_AVAILABLE, conn(c).live, conn(c).banner, not(conn(c).sendsPk), not(conn(c).expired)
  }

  // --- 6. invariants ---------------------------------------------------------
  // S1-S20 safety (design §3.1), P1-P6 permissiveness (§3.2), L1-L8
  // settlement in armed-invariant style (§3.3, FB-1). Every settlement
  // claim is a state invariant of the shape "every state still holding the
  // resource has its designed reclamation path armed/enabled" per the §2d
  // table; the firing/reclamation traces are pinned by the §6b witnesses
  // and Stage-C named runs. The §6c strict variants are pre-registered
  // expected as-built falsifications and are NOT in allInvariants.

  // -- helper predicates --

  /// A send or close-out budget is armed on some session of this connection.
  def connBudgetArmed(c: str): bool =
    CHANS.exists(i => sessAt(c, i).sendBudget or sessAt(c, i).coBudget)

  /// A per-session stage deadline (handshake or opcode-idle) is armed on
  /// some session of this connection.
  def connSessDeadlineArmed(c: str): bool =
    CHANS.exists(i => sessAt(c, i).hsArmed or sessAt(c, i).idleArmed)

  /// L1's armed-path disjunction (§2d): some designed reclamation path for
  /// the connection's current stage is armed or enabled. Stage deadlines
  /// and graces count when armed (their §2d fire conditions protect
  /// permissiveness, not armed-ness); the force-close counts when its
  /// enforcement can reach the connection (the park-ordering bit); the
  /// transport-reap letters count when their §2d enabling condition holds;
  /// the inactivity backstop counts only where the regime admits the letter.
  def reclaimPathArmed(c: str, allowInactivity: bool): bool = or {
    conn(c).preAuth == PaArmed,
    conn(c).grace == GArmed,
    conn(c).grace == GChecked,
    connBudgetArmed(c),
    connSessDeadlineArmed(c),
    fcEnforceable(c),
    conn(c).expired,
    conn(c).herr,
    keepaliveEnabled(c),
    tcpUtoEnabled(c),
    allowInactivity and inactivityEnabled(c)
  }

  // -- 6a.1 capacity, bookkeeping, accounting (S1-S9) --

  /// S1 ConnPermitConservation (gate form): permits in use match the live
  /// holders and never exceed the cap; a connection past an auth callback
  /// that is not being torn down holds a permit and is counted; a
  /// permit-less (over-cap) connection is torn down at its first auth
  /// callback. Reaching that callback permit-less is the legitimate
  /// over-cap path and does not violate.
  val s1ConnPermitConservation = and {
    srv.connPermits == CONNS.filter(c => conn(c).exists and conn(c).permit).size(),
    srv.connPermits <= MAX_CONNECTIONS,
    CONNS.forall(c => and {
      (conn(c).exists and conn(c).live and conn(c).stage != CTcp and not(conn(c).herr))
        implies (conn(c).permit and conn(c).counted),
      (conn(c).exists and conn(c).live and conn(c).stage != CTcp and not(conn(c).permit))
        implies conn(c).herr
    })
  }

  /// S2 SessionPermitConservation: held session permits equal live
  /// SessionGuards and never exceed the session cap.
  val s2SessionPermitConservation = and {
    srv.sessPermits == heldGuards,
    srv.sessPermits <= MAX_SESSIONS
  }

  /// S3 GaugeAccuracy: connections_active counts connections that reached
  /// an auth callback and have not dropped; channels_active equals live
  /// SessionGuards (the guard-held definition of "session").
  val s3GaugeAccuracy = and {
    srv.connsActive == CONNS.filter(c => conn(c).exists and conn(c).counted and conn(c).live).size(),
    srv.chansActive == heldGuards
  }

  /// S4 PerConnLiveCount: each connection's live_sessions equals its live
  /// SessionGuards, up to the increment leaked by the exec-gate panic
  /// splitter (the documented fault-letter weakening).
  val s4PerConnLiveCount =
    CONNS.forall(c => conn(c).liveSess == connGuards(c) + (if (conn(c).live) conn(c).panicLeaks else 0))

  /// S5 NoSessionOutlivesConnection (capacity half): when a connection's
  /// transport has ended, none of its sessions still holds a SessionGuard;
  /// the proto tasks' own exits (after at most the cancel obligation) are
  /// armed by construction and pinned by the teardown witnesses.
  val s5NoSessionOutlivesConnection =
    CONNS.forall(c => (conn(c).exists and not(conn(c).live)) implies CHANS.forall(i => not(sessAt(c, i).guard)))

  /// S6 SingleExecPerChannel: a channel's write half is consumed by at most
  /// one admitted exec, ever.
  val s6SingleExecPerChannel = CHKEYS.forall(k => chans.get(k).execs <= 1)

  /// S7 ChannelAccounting: open_channels equals the accepted-and-open set,
  /// stays within the per-connection bound, and execs are admitted only on
  /// accepted, currently-open channels.
  val s7ChannelAccounting = and {
    CONNS.forall(c => and {
      conn(c).openCh == CHANS.filter(i => chanAt(c, i).ch == ChAccepted).size(),
      conn(c).openCh <= MAX_CHANNELS_PER_CONN
    }),
    not(viol.execOnNonOpenChannel)
  }

  /// S8 RejectReleasesCapacity: an exec gate interrupted by the panic
  /// splitter never strands an acquired permit (no permit held with no
  /// guard and no admission in progress). Outside the panic regime the
  /// content is subsumed by S2/S9 (design scope note).
  val s8RejectReleasesCapacity = not(viol.permitStranded)

  /// S9 SingleRelease: no permit, gauge slot, live-count or map entry is
  /// released twice — no negative counters, no double-release latch.
  val s9SingleRelease = and {
    not(viol.doubleRelease),
    srv.connPermits >= 0, srv.sessPermits >= 0, srv.connsActive >= 0, srv.chansActive >= 0,
    CONNS.forall(c => conn(c).openCh >= 0 and conn(c).liveSess >= 0)
  }

  // -- 6a.2 teardown obligations, force-close, drain (S10-S20) --

  /// S10 CancelOnSessionEnd: on every non-panic session exit with a build
  /// still tracked, a CancelBuild attempt was made before the proto task
  /// ended (cancel_active_builds). The panic carve-out is the documented
  /// weakening; S10-strict in §6c is the probe.
  val s10CancelOnSessionEnd =
    CHKEYS.forall(k =>
      (sessOf(k).exists and sessOf(k).proto == PExited and sessOf(k).exitR != RPanic and sessOf(k).tracked)
        implies sessOf(k).cancelSent)

  /// S11 ForceCloseArmedSticky: an armed force-close is never cleared, and
  /// after expiry no further bytes are accepted from or delivered to the
  /// peer (latches; the as-built guards keep them false). Expiry only ever
  /// follows arming. The numeric earliest-deadline-wins half lives in the
  /// ForceClose fetch_min unit tests, not the model.
  val s11ForceCloseArmedSticky = and {
    not(viol.fcDisarmed),
    not(viol.deliveredAfterExpiry),
    CONNS.forall(c => conn(c).expired implies conn(c).fcArmed)
  }

  /// S12 DecideImpliesArmed: whenever a polite disconnect is queued
  /// (auth-timeout, empty-grace), the force-close is armed at the same
  /// decision point.
  val s12DecideImpliesArmed = CONNS.forall(c => conn(c).discQ implies conn(c).fcArmed)

  /// S13 WindowPacedEgress: data handed to russh never exceeds the
  /// client-granted window plus the bounded handle-queue slot — the
  /// unbounded pendingData path is never used and the queue stays within
  /// its depth, so egress buffering is bounded independent of response
  /// size.
  val s13WindowPacedEgress = and {
    CHKEYS.forall(k => chans.get(k).pending == 0),
    CONNS.forall(c => conn(c).queue <= QUEUE_DEPTH)
  }

  /// S14 RusshResidueBounded: hidden russh channel-table residue stays
  /// within the policy bound — any open that would grow it past the bound
  /// (over-bound or non-session) terminates the connection instead.
  val s14RusshResidueBounded = CONNS.forall(c => conn(c).residue <= RESIDUE_BOUND)

  /// S15 ReleaseBeforeCloseOut: capacity release (guard drop) precedes the
  /// close-out sends — a session in its close-out phase never still holds
  /// its guard, so release never depends on the peer accepting the
  /// close-out or on the handle queue draining.
  val s15ReleaseBeforeCloseOut =
    CHKEYS.forall(k => (sessOf(k).exists and sessOf(k).eg == EgCloseOut) implies not(sessOf(k).guard))

  /// S16 TrackedBuildPolicy (as shipped): a build leaves the tracked set
  /// only when a terminal outcome was processed or the stream error class
  /// was non-Wire (the documented trade-off); never merely because the
  /// session is ending. The strict variant in §6c is the W10 probe.
  val s16TrackedBuildPolicy =
    CHKEYS.forall(k => (sessOf(k).everTracked and not(sessOf(k).tracked))
      implies (sessOf(k).terminal or sessOf(k).nonWireErr))

  /// S17 DrainStageOrder: the shutdown stages advance only in order
  /// (NOT_SERVING ≺ accept-stop ≺ sessions-done/expiry ≺ cancel ≺ exit)
  /// and accept-stop terminates no established connection or session.
  val s17DrainStageOrder = and {
    not(viol.drainOrderViolated),
    not(viol.acceptStopKilled)
  }

  /// S18 ListenerSurvivesAcceptErrors: transient accept errors (EMFILE
  /// class) never stop the accept loop or the process.
  val s18ListenerSurvivesAcceptErrors = not(viol.listenerDied)

  /// S19 CloseOutOrder: the close-out attempts exit-status before eof
  /// before close; delivery is best-effort and bounded by the close-out
  /// budget (S15/L2 carry the budget half).
  val s19CloseOutOrder = CHKEYS.forall(k => and {
    sessOf(k).coEof implies sessOf(k).coStatus,
    sessOf(k).coClose implies sessOf(k).coEof
  })

  /// S20 GraceArmedExactlyWhenEmpty: the grace is armed only on an
  /// authenticated connection with zero live sessions (channel opens are
  /// not activity), and a live healthy authenticated connection with zero
  /// live sessions whose grace has not fired has it armed (or mid-check).
  /// Admission disarms it.
  val s20GraceArmedExactlyWhenEmpty = CONNS.forall(c => and {
    (conn(c).grace == GArmed) implies and {
      conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
      conn(c).liveSess == 0
    },
    and {
      conn(c).exists, conn(c).live, not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
      conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
      conn(c).liveSess == 0,
      conn(c).grace != GFired
    } implies (conn(c).grace == GArmed or conn(c).grace == GChecked)
  })

  // -- 6a.3 permissiveness (P1-P6): guards must not fire on legitimate traces --

  /// P1 MuxFanOutAdmitted: a single connection with in-bounds channel count
  /// gets every exec admitted while global capacity remains — no
  /// per-connection refusal below the global cap.
  val p1MuxFanOutAdmitted = not(viol.execRefusedBelowCap)

  /// P2 SurvivesSessionCountZero: a connection whose live-session count
  /// touches zero is not disconnected before the grace fires.
  val p2SurvivesSessionCountZero = not(viol.disconnectOnTouchZero)

  /// P3 CapRejectsExecOnly: capacity exhaustion is signaled by exec
  /// rejection (channel_failure + exit-status) only — never by refusing the
  /// channel open, never by terminating the connection.
  val p3CapRejectsExecOnly = not(viol.capRefusedOpenOrConn)

  /// P4 WithinGraceExecSurvives: a connection that admits an exec before
  /// the grace re-check passes is not disconnected by that grace cycle (the
  /// post-check admit window is the documented W3 race, carved out — see
  /// the L5-strict probe).
  val p4WithinGraceExecSurvives = not(viol.gracePreCheckAdmitKilled)

  /// P5 CompliantPeerNotForceClosed: against a connection whose peer has
  /// played only legitimate letters, no send/close-out budget fires, no
  /// keepalive / TCP_USER_TIMEOUT / inactivity reap fires, no force-close
  /// is armed by a budget site, and no well-formed exec is rejected below
  /// the global cap. (The empty grace may still fire on an idle compliant
  /// connection — that is the as-built design, not a wedge response.)
  val p5CompliantPeerNotForceClosed = and {
    not(viol.budgetFiredOnCompliant),
    not(viol.timerReapedCompliant),
    not(viol.execRefusedBelowCap),
    CONNS.forall(c => (conn(c).exists and compliant(c)) implies not(conn(c).fcByBudget))
  }

  /// P6 EstablishedSurviveAcceptStop: stopping the accept loop (drain stage
  /// 2) terminates no established connection or session before the drain
  /// window elapses.
  val p6EstablishedSurviveAcceptStop = not(viol.acceptStopKilled)

  // -- 6a.4 settlement (L1-L8): armed-style state invariants --

  /// L1 ConnReclaimArmed: every connection holding capacity (fd, permit,
  /// gauge slot, channel state) with no live admitted session has a
  /// designed reclamation path armed/enabled for its current stage; the
  /// inactivity backstop is acceptable only where the regime admits the
  /// letter (fault-degraded — the W7 degraded tier).
  val l1ConnReclaimArmed =
    CONNS.forall(c => (conn(c).exists and conn(c).live and conn(c).liveSess == 0)
      implies reclaimPathArmed(c, true))

  /// L2 SendSettleArmed: every state where a session's output has ended, or
  /// a send / close-out is pending toward a transport-withholding peer, has
  /// the capacity already released or a send/close-out budget, an
  /// enforceable force-close, or the pump's own exit armed for it — release
  /// never requires a further peer letter (the wedge response).
  val l2SendSettleArmed = CHKEYS.forall(k =>
    (sessOf(k).exists and or {
      sessOf(k).proto == PExited,
      sessOf(k).eg == EgSendPend and conns.get(k._1).trW,
      sessOf(k).eg == EgCloseOut
    }) implies or {
      not(sessOf(k).guard),
      sessOf(k).sendBudget,
      sessOf(k).coBudget,
      fcEnforceable(k._1),
      sessOf(k).proto == PExited and (sessOf(k).eg == EgFwd or sessOf(k).eg == EgSendPend)
    })

  /// L3 PreSessionOccupancyArmed: a session occupied pre-handshake has the
  /// handshake deadline armed; a session idle between opcodes has the
  /// opcode-idle deadline armed. Firing exits the occupancy (witness
  /// pinned).
  val l3PreSessionOccupancyArmed = CHKEYS.forall(k => and {
    (sessOf(k).exists and sessOf(k).proto == PHandshaking) implies sessOf(k).hsArmed,
    (sessOf(k).exists and sessOf(k).proto == PReady) implies sessOf(k).idleArmed
  })

  /// L4 DrainObligationsArmed (exit-quiescence half): the exit stage is
  /// reachable only with zero remaining proto tasks and zero remaining
  /// counted connections / permits. The per-stage "shutdown path enabled
  /// without further client traffic" half is by construction (the fired
  /// token enables every proto's observe action) and is pinned by the
  /// drain named run.
  val l4DrainObligationsArmed =
    (srv.phase == SrvExited) implies and {
      liveProtos == 0,
      srv.connsActive == 0,
      srv.connPermits == 0
    }

  /// L5 GraceNeverKillsLiveSession: the grace disconnect is never taken in
  /// a state with a live SessionGuard outside the documented post-check
  /// admit race (the carve-out); the un-carved form is the §6c probe.
  val l5GraceNeverKillsLiveSession = not(viol.graceKilledLiveSession)

  /// L6 RpcWaitDeadlineArmed: every session in in-opcode-rpc-wait has that
  /// wait's upstream deadline armed (every unary/store RPC await carries a
  /// deadline — the F9 surface). Deliberately NOT claimed for
  /// in-opcode-build-event-wait, which has no deadline by design.
  val l6RpcWaitDeadlineArmed =
    CHKEYS.forall(k => (sessOf(k).exists and sessOf(k).proto == POpRpc) implies sessOf(k).rpcDl)

  /// L7 UpstreamStreamReleasedOnExit: once a session's proto-phase has
  /// exited and its finish/abort has run, the per-build upstream-stream
  /// bit is cleared — the obligation is discharged by session exit itself,
  /// with no peer letter (the W10 decision-rule observable).
  val l7UpstreamStreamReleasedOnExit =
    CHKEYS.forall(k =>
      (sessOf(k).exists and sessOf(k).proto == PExited
        and (sessOf(k).eg == EgDone or sessOf(k).eg == EgAborted))
      implies not(sessOf(k).streamHeld))

  /// L8 SessionTaskQuiescence: a session whose proto task has exited and
  /// whose finish phase completed has no session-owned tasks left (the
  /// GW-11 mux surface). A session whose pump is still unreaped always has
  /// a finish/reap or self-exit action enabled without any peer letter —
  /// by construction in the as-built encoding (every non-EgDone egress
  /// state has one), so the reap omission is what a calibration override
  /// breaks.
  val l8SessionTaskQuiescence =
    CHKEYS.forall(k =>
      (sessOf(k).exists and sessOf(k).proto == PExited and sessOf(k).eg == EgDone)
        implies not(sessOf(k).pump))

  /// Bounds discipline: every counter stays inside its ceiling (the
  /// ceilings are action preconditions, so this doubles as a check that no
  /// action forgot one).
  val boundsOK = and {
    CONNS.forall(c => and {
      conn(c).queue >= 0, conn(c).queue <= QUEUE_DEPTH,
      conn(c).residue >= 0, conn(c).residue <= RESIDUE_CEIL,
      conn(c).openCh >= 0, conn(c).openCh <= MAX_CHANNELS_PER_CONN,
      conn(c).liveSess >= 0, conn(c).liveSess <= MAX_SESSIONS + conn(c).panicLeaks,
      conn(c).panicLeaks >= 0, conn(c).panicLeaks <= 1
    }),
    CHKEYS.forall(k => and {
      chans.get(k).window >= 0, chans.get(k).window <= WINDOW_CREDITS,
      chans.get(k).pending >= 0, chans.get(k).pending <= PENDING_CEIL,
      chans.get(k).execs >= 0, chans.get(k).execs <= 1
    }),
    srv.connPermits >= 0, srv.connPermits <= MAX_CONNECTIONS,
    srv.sessPermits >= 0, srv.sessPermits <= MAX_SESSIONS,
    srv.connsActive >= 0, srv.chansActive >= 0
  }

  /// The default invariant conjunction the wired regime checks assert
  /// (Stage C): all 34 design-§3 properties plus the bounds discipline.
  /// Witnesses (§6b) and the pre-registered strict probes (§6c) are wired
  /// separately and are NOT part of this conjunction.
  val allInvariants = and {
    s1ConnPermitConservation, s2SessionPermitConservation, s3GaugeAccuracy,
    s4PerConnLiveCount, s5NoSessionOutlivesConnection, s6SingleExecPerChannel,
    s7ChannelAccounting, s8RejectReleasesCapacity, s9SingleRelease,
    s10CancelOnSessionEnd, s11ForceCloseArmedSticky, s12DecideImpliesArmed,
    s13WindowPacedEgress, s14RusshResidueBounded, s15ReleaseBeforeCloseOut,
    s16TrackedBuildPolicy, s17DrainStageOrder, s18ListenerSurvivesAcceptErrors,
    s19CloseOutOrder, s20GraceArmedExactlyWhenEmpty,
    p1MuxFanOutAdmitted, p2SurvivesSessionCountZero, p3CapRejectsExecOnly,
    p4WithinGraceExecSurvives, p5CompliantPeerNotForceClosed, p6EstablishedSurviveAcceptStop,
    l1ConnReclaimArmed, l2SendSettleArmed, l3PreSessionOccupancyArmed,
    l4DrainObligationsArmed, l5GraceNeverKillsLiveSession, l6RpcWaitDeadlineArmed,
    l7UpstreamStreamReleasedOnExit, l8SessionTaskQuiescence,
    boundsOK
  }

  // --- 6b. expect-violation witnesses (non-vacuity probes, design §5) --------
  // Each is "the interesting state never happens"; a wired witness check
  // passes only when the checker violates it, re-proving the contended
  // state is reachable in that regime's explored space.

  /// A stalled response send arms the force-close (C14 / GW-2 surface).
  val canReachStallArmsForceClose = not(reach.stallFcArmed)
  /// A server-side session ending releases capacity while the channel is
  /// still open and the connection still live (W2 / guard-held semantics).
  val canReachServerSideEndingReleasesEarly = not(reach.releaseWithChannelOpen)
  /// The empty-connection grace fires on an idle authenticated connection.
  val canReachGraceFiresOnIdleConn = not(reach.graceFired)
  /// An exec admitted while the grace was armed disarms it (P4 happy half).
  val canReachExecWithinGraceSurvives = not(reach.execDisarmedGrace)
  /// A KEX-parked (or otherwise pre-auth-withholding) connection is
  /// reclaimed (GW-3/GW-4 surface).
  val canReachKexParkedReclaimed = not(reach.kexParkedReclaimed)
  /// A forged/duplicate channel close is ignored (GW-5 surface).
  val canReachForgedCloseIgnored = not(reach.forgedCloseIgnored)
  /// An over-bound channel open terminates the connection (S14 surface).
  val canReachOverBoundOpenTerminates = not(reach.overBoundTerminated)
  /// A burst of opens reaches the per-connection bound (GW-6 surface).
  val canReachBurstHitsBound = not(reach.burstHitBound)
  /// A session-cap exec rejection occurs while the channel open succeeded
  /// (P3 / GW-18 surface).
  val canReachSessionCapExecRejected = not(reach.capExecRejected)
  /// An over-cap connection reaches its first auth callback permit-less and
  /// is torn down (S1 cap clause / GW-12; cap regime).
  val canReachOverCapAuthTorn = not(reach.overCapAuthTorn)
  /// A drain-expiry shutdown-token cancel fires on a tracked build (L4/S17).
  val canReachDrainExpiryCancel = not(reach.drainCancelFired)
  /// A connection touches zero live sessions and execs again (P1/P2 mux).
  val canReachMuxTouchZeroThenExec = not(reach.muxReExec)
  /// A session reaches tasks==0 while a sibling session on the same
  /// connection stays live (L8 / GW-11 mux surface).
  val canReachMuxSiblingQuiescence = not(reach.muxSiblingQuiesced)
  /// A half-open vanish is reclaimed by a designed transport-reap letter
  /// (W5 occupancy bound).
  val canReachVanishReclaimedDesigned = not(reach.vanishReclaimedDesigned)
  /// A parked-write connection is reclaimed via the inactivity letter (the
  /// W7 degraded tier; fault-degraded regime only).
  val canReachParkedReclaimedByInactivity = not(reach.parkedReclaimedByInactivity)
  /// A non-Wire stream error removes a tracked build (S16 shipped policy).
  val canReachNonWireRemovesTracked = not(reach.nonWireRemoved)
  /// A close-out completes in order (exit-status, eof, close — S19).
  val canReachCloseOutCompletesInOrder = not(reach.closeOutOrdered)

  // --- 6c. pre-registered expected as-built falsifications (design §3/§4/§6) -
  // Named probes, NOT in allInvariants. Stage C wires each as an
  // expect-violation check in the regime named by the invariant map; a
  // probe that stops falsifying means the model (or the code) moved.

  /// L1 without the inactivity disjunct: expected to falsify only in
  /// fault-degraded (the W7 ~1 h degraded tier — a parked write whose
  /// force-close was armed after the park, with TCP_USER_TIMEOUT
  /// unavailable, is reclaimed by nothing but the inactivity backstop).
  val l1StrictNoInactivity =
    CONNS.forall(c => (conn(c).exists and conn(c).live and conn(c).liveSess == 0)
      implies reclaimPathArmed(c, false))

  /// S16 strict variant (the W10 probe): a build leaves the tracked set
  /// only on a processed terminal outcome. Expected to falsify on the
  /// non-Wire removal trace (fault-upstream) — the orphan-watcher-bounded
  /// leak the owner signs off on.
  val s16StrictTerminalOnly =
    CHKEYS.forall(k => (sessOf(k).everTracked and not(sessOf(k).tracked)) implies sessOf(k).terminal)

  /// S10 strict variant: a CancelBuild attempt on every exit including the
  /// panic path. Expected to falsify in fault-degraded (the panic letter);
  /// the scheduler orphan watcher is the named backstop assumption.
  val s10StrictIncludingPanic =
    CHKEYS.forall(k => (sessOf(k).exists and sessOf(k).proto == PExited and sessOf(k).tracked)
      implies sessOf(k).cancelSent)

  /// L5 without the P4 carve-out: the grace never fires with a live
  /// SessionGuard at all. Expected to falsify in base (the documented
  /// post-check admit race, W3).
  val l5StrictNoCarveOut = not(reach.graceFiredWithLiveSession)

  // --- 7. actions ------------------------------------------------------------

  // --- 7a. accept and SSH establishment (legitimate alphabet) ----------------

  /// TCP accept (server/mod.rs accept select → new_client): the conn permit
  /// is try-acquired (over cap ⇒ no permit, rejection deferred to the first
  /// auth callback), the pre-auth deadline is armed from accept, the
  /// ForceClose slot exists unarmed. Accept continues during drain stage 1
  /// (NOT_SERVING) and stops at stage 2 (accept-stop).
  action acceptConn(c: str): bool = all {
    or { srv.phase == SrvServing, srv.phase == SrvNotServing },
    not(conn(c).exists),
    val gotPermit = srv.connPermits < MAX_CONNECTIONS
    all {
      conns' = conns.set(c, { ...freshConn, exists: true, live: true, permit: gotPermit, preAuth: PaArmed }),
      srv' = { ...srv, connPermits: if (gotPermit) srv.connPermits + 1 else srv.connPermits },
      chans' = chans,
      viol' = viol,
      reach' = reach
    }
  }

  /// The peer completes the SSH version exchange and initial KEX (the
  /// compliant establishment path; russh internals, no handler callback).
  action peerBanner(c: str): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).banner), not(conn(c).herr), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), banner: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// An auth callback that does not authenticate (auth_none probe, unknown
  /// key, rejected offer): mark_real_connection latches the gauge/count on
  /// the first callback; ensure_permit tears the connection down if the
  /// accept-time permit was missing (the S1 gate form). The connection
  /// otherwise stays up, still bounded by the pre-auth deadline.
  action peerAuthFail(c: str): bool = all {
    c == ADV, LET_OCC,
    conn(c).exists, conn(c).live, conn(c).banner, not(conn(c).kex),
    conn(c).stage == CTcp or conn(c).stage == CAuthAtt,
    not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    val firstCb = not(conn(c).counted)
    val noPermit = not(conn(c).permit)
    all {
      conns' = conns.set(c, { ...conn(c),
        stage: CAuthAtt,
        counted: true,
        occW: true,
        herr: conn(c).herr or noPermit
      }),
      srv' = { ...srv, connsActive: if (firstCb) srv.connsActive + 1 else srv.connsActive },
      chans' = chans, viol' = viol,
      reach' = { ...reach, overCapAuthTorn: reach.overCapAuthTorn or noPermit }
    }
  }

  /// auth_publickey accepts (key in authorized_keys, optional ResolveTenant
  /// + JWT mint): the gauge/count latch on the first callback, the pre-auth
  /// deadline latches off, the empty-connection grace arms (zero live
  /// sessions). An over-cap (permit-less) connection is torn down by
  /// ensure_permit at this same callback instead of authenticating.
  action peerAuthOk(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).banner, not(conn(c).kex),
    conn(c).stage == CTcp or conn(c).stage == CAuthAtt,
    not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    val firstCb = not(conn(c).counted)
    val noPermit = not(conn(c).permit)
    all {
      conns' = conns.set(c,
        if (noPermit)
          { ...conn(c), stage: CAuthAtt, counted: true, herr: true }
        else
          { ...conn(c), stage: CAuthed, counted: true, preAuth: PaLatched, grace: GArmed }),
      srv' = { ...srv, connsActive: if (firstCb) srv.connsActive + 1 else srv.connsActive },
      chans' = chans, viol' = viol,
      reach' = { ...reach, overCapAuthTorn: reach.overCapAuthTorn or noPermit }
    }
  }

  /// channel_open_session within the per-connection bound: the channel is
  /// accepted, its write half is registered, open_channels increments.
  /// ConnStage moves to ChannelOpen when the callback is invoked. Channel
  /// opens are NOT activity for the empty-connection grace.
  action peerOpenChannel(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    chanAt(c, i).ch == ChNone,
    conn(c).openCh < MAX_CHANNELS_PER_CONN,
    conns' = conns.set(c, { ...conn(c), stage: CChanOpen, openCh: conn(c).openCh + 1 }),
    chans' = chans.set((c, i), { ...chanAt(c, i), ch: ChAccepted, writeHalf: true }),
    srv' = srv, viol' = viol,
    reach' = { ...reach, burstHitBound: reach.burstHitBound or conn(c).openCh + 1 == MAX_CHANNELS_PER_CONN }
  }

  /// channel_open_session over the per-connection bound: the handler bails
  /// (anyhow::bail!) and the connection is terminated; the russh-side
  /// channel-table residue entry is the price (S14 keeps it bounded by
  /// terminating). The attempt letter needs no free slot (§2e).
  action peerOpenOverBound(c: str): bool = all {
    c == ADV, LET_HOSTILE,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    conn(c).openCh >= MAX_CHANNELS_PER_CONN,
    conn(c).residue < RESIDUE_CEIL,
    conns' = conns.set(c, { ...conn(c), stage: CChanOpen, herr: true, residue: conn(c).residue + 1 }),
    chans' = chans, srv' = srv, viol' = viol,
    reach' = { ...reach, overBoundTerminated: true }
  }

  /// A non-`session` channel open (direct-tcpip / x11 / ...): rejected by
  /// terminating the connection (reject_unsupported_channel_open), so the
  /// russh residue cannot grow unbounded.
  action peerOpenNonSession(c: str): bool = all {
    c == ADV, LET_HOSTILE,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).residue < RESIDUE_CEIL,
    conns' = conns.set(c, { ...conn(c), herr: true, residue: conn(c).residue + 1 }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// A CHANNEL_CLOSE for an id that was never accepted (forged or
  /// duplicate): note_channel_closed ignores it — no decrement, no map
  /// mutation (GW-5 surface).
  action peerForgedClose(c: str): bool = all {
    c == ADV, LET_HOSTILE,
    conn(c).exists, conn(c).live, not(conn(c).herr), not(conn(c).expired),
    conns' = conns, chans' = chans, srv' = srv, viol' = viol,
    reach' = { ...reach, forgedCloseIgnored: true }
  }

  // --- 7b. exec admission and rejection (the session-cap gate) ---------------

  /// exec_request admitted (one atomic action per ground rule §2d-2):
  /// session permit acquired, write half consumed, live_sessions++ and the
  /// grace disarmed (session_admitted), channel_success, gauge++, guard
  /// created, proto/response/client-pump tasks spawned.
  action execAdmit(c: str, i: int): bool = all {
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
      chans' = chans.set((c, i), { ...chanAt(c, i), writeHalf: false, execs: chanAt(c, i).execs + 1, sess: admittedSess }),
      srv' = { ...srv, sessPermits: srv.sessPermits + 1, chansActive: srv.chansActive + 1 },
      viol' = viol,
      reach' = { ...reach,
        execDisarmedGrace: reach.execDisarmedGrace or wasArmed,
        muxReExec: reach.muxReExec or conn(c).sawZero
      }
    }
  }

  /// exec_request rejected at the session cap: channel_failure + exit
  /// status 1 + eof + close toward the client only — the server-side
  /// channel state is untouched (the channel can be exec'd later when
  /// capacity frees), the connection survives, the channel open was never
  /// refused (P3).
  action execRejectCap(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChAccepted,
    chanAt(c, i).writeHalf,
    not(sessAt(c, i).exists),
    srv.sessPermits >= MAX_SESSIONS,
    conns' = conns, chans' = chans, srv' = srv, viol' = viol,
    reach' = { ...reach, capExecRejected: true }
  }

  /// The exec-gate panic splitter (fault letter, design §2d-2): the gate is
  /// interrupted after session_admitted (live_sessions incremented, grace
  /// disarmed, write half taken) and before guard creation. The locally
  /// held permit is released by unwind (never stranded — S8); the
  /// live_sessions increment leaks until connection teardown (S4's panic
  /// term); the handler error tears the connection down.
  action execGatePanic(c: str, i: int): bool = all {
    c == ADV, LET_PANIC,
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    chanAt(c, i).ch == ChAccepted,
    chanAt(c, i).writeHalf,
    not(sessAt(c, i).exists),
    srv.sessPermits < MAX_SESSIONS,
    conn(c).panicLeaks == 0,
    conns' = conns.set(c, { ...conn(c),
      liveSess: conn(c).liveSess + 1,
      panicLeaks: conn(c).panicLeaks + 1,
      grace: if (conn(c).grace == GArmed) GUnarmed else conn(c).grace,
      herr: true
    }),
    chans' = chans.set((c, i), { ...chanAt(c, i), writeHalf: false }),
    srv' = srv, viol' = viol, reach' = reach
  }

  // --- 7c. protocol session: proto-task phases -------------------------------

  /// The client completes the nix-daemon handshake (WORKER_MAGIC_1 /
  /// version exchange): pre-handshake → ready, the opcode idle timer arms.
  action peerHandshake(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == PHandshaking,
    sessAt(c, i).pump, not(chanAt(c, i).clientEof),
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: PReady, hsArmed: false, idleArmed: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The client sends the next opcode: ready → in-opcode-client-read (the
  /// handler may read further client data); the idle timer stops covering
  /// in-opcode time (OPCODE_IDLE_TIMEOUT excludes it by design).
  action peerOpcode(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == PReady,
    sessAt(c, i).pump, not(chanAt(c, i).clientEof),
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpRead, idleArmed: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The opcode issues a unary/store upstream RPC (every such await is
  /// with_timeout-wrapped — the F9 surface): in-opcode-rpc-wait with its
  /// deadline armed (L6).
  action opcodeToRpcWait(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpRead,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpRpc, rpcDl: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// A local opcode that needs no upstream call: straight to emitting.
  action opcodeToEmit(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpRead,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpEmit } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The upstream RPC completes (store op): rpc-wait → emitting.
  action rpcCompletesStore(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpRpc,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpEmit, rpcDl: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// SubmitBuild accepted: the build is tracked in active_build_ids, the
  /// BuildEvent stream is held, and the session parks in
  /// in-opcode-build-event-wait — which deliberately has NO deadline arm.
  action rpcCompletesBuildSubmit(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpRpc,
    not(sessAt(c, i).everTracked),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: POpBuild, rpcDl: false, tracked: true, everTracked: true, streamHeld: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The rpc-wait deadline fires (scheduler/store slow or unavailable —
  /// upstream fault): the RPC fails, the error is reported to the client
  /// via stderr, the opcode completes (the session survives).
  action fireRpcDeadline(c: str, i: int): bool = all {
    LET_UPSTREAM,
    sessAt(c, i).exists, sessAt(c, i).proto == POpRpc, sessAt(c, i).rpcDl,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: POpEmit, rpcDl: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// A terminal build outcome is processed: the build leaves the tracked
  /// set (terminal class), the handler returns and the BuildEvent stream
  /// drops, the result is emitted.
  action buildTerminal(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpBuild,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: POpEmit, tracked: false, terminal: true, streamHeld: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The BuildEvent stream fails with a non-Wire error class (upstream
  /// fault, e.g. reconnect exhausted): the failure is reported to the
  /// client and the build is removed from tracking — the documented
  /// trade-off S16 encodes (the strict variant is the W10 probe).
  action buildStreamErrorNonWire(c: str, i: int): bool = all {
    LET_UPSTREAM,
    sessAt(c, i).exists, sessAt(c, i).proto == POpBuild, sessAt(c, i).tracked,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: POpEmit, tracked: false, nonWireErr: true, streamHeld: false } }),
    conns' = conns, srv' = srv, viol' = viol,
    reach' = { ...reach, nonWireRemoved: true }
  }

  /// The opcode's response is fully emitted: back to ready, idle timer
  /// re-arms.
  action emitDone(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == POpEmit,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: PReady, idleArmed: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// channel_eof: the client half-closes the channel; client_tx is taken,
  /// the client pump will see None and exit; the proto reader sees EOF the
  /// next time it reads.
  action peerClientEof(c: str, i: int): bool = all {
    chanAt(c, i).ch == ChAccepted, not(chanAt(c, i).clientEof),
    conn(c).live, not(conn(c).expired), not(conn(c).discQ), not(conn(c).herr),
    chans' = chans.set((c, i), { ...chanAt(c, i), clientEof: true }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The proto task reads EOF between opcodes / while reading client data
  /// (clean client disconnect, P7): cancel obligation then exit.
  action protoSeesClientEof(c: str, i: int): bool = all {
    sessAt(c, i).exists,
    sessAt(c, i).proto == PReady or sessAt(c, i).proto == POpRead or sessAt(c, i).proto == PHandshaking,
    or { chanAt(c, i).clientEof, not(sessAt(c, i).pump) },
    val s = sessAt(c, i)
    val reason = if (s.proto == PHandshaking) RHandshakeFail else RClientEof
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(s, reason) }),
      conns' = conns, srv' = srv, viol' = viol, reach' = reach
    }
  }

  /// Garbage on the wire between opcodes (malformed opcode, RST-induced
  /// read error — the P8 wire-error class).
  action peerWireGarbage(c: str, i: int): bool = all {
    c == ADV, LET_HOSTILE,
    sessAt(c, i).exists,
    sessAt(c, i).proto == PReady or sessAt(c, i).proto == POpRead,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(sessAt(c, i), RWire) }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// Mid-opcode handler error (P10): the response pump is gone (BrokenPipe
  /// into the outbound duplex) or the connection died under the opcode. A
  /// Wire-class failure keeps the build tracked so the cancel loop covers
  /// it (the S16 keep arm).
  action protoHandlerError(c: str, i: int): bool = all {
    sessAt(c, i).exists,
    sessAt(c, i).proto == POpRead or sessAt(c, i).proto == POpRpc or
      sessAt(c, i).proto == POpBuild or sessAt(c, i).proto == POpEmit,
    or {
      sessAt(c, i).eg == EgCloseOut, sessAt(c, i).eg == EgDone, sessAt(c, i).eg == EgAborted,
      not(conn(c).live), conn(c).expired, chanAt(c, i).ch == ChClosed
    },
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(sessAt(c, i), RHandlerErr) }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The proto task observes the per-channel shutdown token (channel close
  /// or server drain stage 3): the in-flight opcode future is dropped, the
  /// cancel obligation runs, the task exits (P5/P9).
  action protoObservesShutdown(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto != PExited, sessAt(c, i).proto != PCancelling,
    or { srv.phase == SrvTokenFired, chanAt(c, i).ch == ChClosed },
    val s = sessAt(c, i)
    val reason = if (s.proto == PHandshaking) RPreHandshake else RShutdown
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(s, reason) }),
      conns' = conns, srv' = srv, viol' = viol,
      reach' = { ...reach, drainCancelFired: reach.drainCancelFired or (s.tracked and srv.phase == SrvTokenFired) }
    }
  }

  /// The proto task observes the connection teardown (its duplex peer
  /// halves are gone after the handler dropped): cancel obligation then
  /// exit — the cancel-on-disconnect path.
  action protoObservesConnGone(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto != PExited, sessAt(c, i).proto != PCancelling,
    not(conn(c).live),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(sessAt(c, i), RClientEof) }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The opcode idle timeout fires (600 s class): only between opcodes,
  /// never against a compliant peer (its next-opcode letter completes
  /// first, §2d).
  action fireOpcodeIdle(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == PReady, sessAt(c, i).idleArmed,
    not(compliant(c)),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(sessAt(c, i), RIdle) }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The protocol handshake timeout fires (30 s class): only pre-handshake,
  /// never against a compliant peer (its handshake letter completes first,
  /// §2d).
  action fireHandshakeTimeout(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == PHandshaking, sessAt(c, i).hsArmed,
    not(compliant(c)),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: protoExit(sessAt(c, i), RHandshakeFail) }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The cancel loop finishes (bounded per build by DEFAULT_GRPC_TIMEOUT;
  /// best-effort — a blackholed CancelBuild is the timeout path of the same
  /// action): the CancelBuild attempt is recorded and the proto task ends.
  action cancelLoopDone(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).proto == PCancelling,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), proto: PExited, cancelSent: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// A handler panic unwinds the proto task (P12, fault letter): the cancel
  /// loop never runs (the tracked build leaks to the scheduler
  /// orphan-watcher backstop), the stream drops with the unwound frame,
  /// capacity is untouched here because the SessionGuard lives on the
  /// response task.
  action protoPanic(c: str, i: int): bool = all {
    c == ADV, LET_PANIC,
    sessAt(c, i).exists,
    sessAt(c, i).proto == PReady or sessAt(c, i).proto == POpRead or sessAt(c, i).proto == POpRpc or
      sessAt(c, i).proto == POpBuild or sessAt(c, i).proto == POpEmit,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      proto: PExited, exitR: RPanic, hsArmed: false, idleArmed: false, rpcDl: false, streamHeld: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  // --- 7d. protocol session: egress, finish, close-out -----------------------

  /// The response pump performs a window-aware send with credit available
  /// (SessionSink on ChannelWriteHalf): one credit consumed, the data is
  /// delivered into russh's bounded machinery — never the unbounded
  /// pendingData path (S13).
  action pumpSendOk(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PHandshaking or sessAt(c, i).proto == POpEmit,
    chanAt(c, i).window > 0,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), window: chanAt(c, i).window - 1 }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The window is exhausted: the send parks at the pump level
  /// (send-pending) under the HANDLE_SEND_TIMEOUT budget.
  action pumpSendBlocks(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PHandshaking or sessAt(c, i).proto == POpEmit,
    chanAt(c, i).window == 0,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgSendPend, sendBudget: true } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The peer grants window (WINDOW_ADJUST): a pending send completes, or a
  /// credit is banked.
  action peerWindowAdjust(c: str, i: int): bool = all {
    chanAt(c, i).ch == ChAccepted,
    conn(c).live, not(conn(c).expired), not(conn(c).herr), not(conn(c).discQ),
    or { chanAt(c, i).window < WINDOW_CREDITS, sessAt(c, i).eg == EgSendPend },
    chans' = chans.set((c, i),
      if (sessAt(c, i).eg == EgSendPend)
        { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgFwd, sendBudget: false } }
      else
        { ...chanAt(c, i), window: chanAt(c, i).window + 1 }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The send budget (HANDLE_SEND_TIMEOUT) fires on a pending send toward a
  /// transport-withholding peer (§2d: never against a compliant peer): the
  /// pump exits stalled, the SessionGuard drops FIRST (capacity released),
  /// the close-out begins under its own budget, and the force-close is
  /// armed (the wedge response, C14).
  action fireSendBudget(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgSendPend, sessAt(c, i).sendBudget,
    conn(c).trW,
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

  /// The pump observes outbound EOF (the proto task has exited and dropped
  /// its writer): finish_channel_session — guard drop first, then the
  /// bounded close-out.
  action pumpExitEof(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgFwd,
    sessAt(c, i).proto == PExited,
    sessAt(c, i).guard,
    conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgCloseOut, guard: false, coBudget: true } }),
    conns' = conns.set(c, connAfterGuardRelease(conn(c))),
    srv' = { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 },
    viol' = viol,
    reach' = { ...reach,
      releaseWithChannelOpen: reach.releaseWithChannelOpen or (chanAt(c, i).ch == ChAccepted and conn(c).live)
    }
  }

  /// The pump's send fails outright (transport already dead/expired or the
  /// channel closed): capacity is released, the close-out is skipped (the
  /// sends would fail), no force-close arm (matches the send-failure break,
  /// which arms nothing).
  action pumpExitSendFail(c: str, i: int): bool = all {
    sessAt(c, i).exists,
    sessAt(c, i).eg == EgFwd or sessAt(c, i).eg == EgSendPend,
    sessAt(c, i).guard,
    or { not(conn(c).live), conn(c).expired, chanAt(c, i).ch == ChClosed },
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i),
      eg: EgDone, guard: false, sendBudget: false, pump: false } }),
    conns' = conns.set(c, connAfterGuardRelease(conn(c))),
    srv' = { ...srv, sessPermits: srv.sessPermits - 1, chansActive: srv.chansActive - 1 },
    viol' = viol, reach' = reach
  }

  /// Close-out send 1/3: exit-status (finish_channel_session, ordered
  /// before eof and close), riding the handle queue.
  action closeOutSendStatus(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, not(sessAt(c, i).coStatus),
    conn(c).queue < QUEUE_DEPTH, conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), coStatus: true } }),
    conns' = conns.set(c, { ...conn(c), queue: conn(c).queue + 1 }),
    srv' = srv, viol' = viol, reach' = reach
  }

  /// Close-out send 2/3: eof, only after exit-status.
  action closeOutSendEof(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, sessAt(c, i).coStatus, not(sessAt(c, i).coEof),
    conn(c).queue < QUEUE_DEPTH, conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), coEof: true } }),
    conns' = conns.set(c, { ...conn(c), queue: conn(c).queue + 1 }),
    srv' = srv, viol' = viol, reach' = reach
  }

  /// Close-out send 3/3: close, only after eof.
  action closeOutSendClose(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, sessAt(c, i).coEof, not(sessAt(c, i).coClose),
    conn(c).queue < QUEUE_DEPTH, conn(c).live, not(conn(c).expired),
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), coClose: true } }),
    conns' = conns.set(c, { ...conn(c), queue: conn(c).queue + 1 }),
    srv' = srv, viol' = viol, reach' = reach
  }

  /// The close-out completed (all three queued within budget): the client
  /// pump is aborted and reaped; the session's finish phase is done.
  action closeOutComplete(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut,
    sessAt(c, i).coStatus, sessAt(c, i).coEof, sessAt(c, i).coClose,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgDone, coBudget: false, pump: false } }),
    conns' = conns, srv' = srv, viol' = viol,
    reach' = { ...reach,
      closeOutOrdered: true,
      muxSiblingQuiesced: reach.muxSiblingQuiesced or
        (sessAt(c, i).proto == PExited and CHANS.exists(j => j != i and chans.get((c, j)).sess.guard))
    }
  }

  /// The close-out budget fires toward a transport-withholding peer: the
  /// close-out is abandoned, the force-close armed, the pump reaped.
  action fireCloseOutBudget(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut, sessAt(c, i).coBudget,
    conn(c).trW,
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgDone, coBudget: false, pump: false } }),
    conns' = conns.set(c, { ...conn(c), fcArmed: true, fcByBudget: true }),
    srv' = srv,
    viol' = { ...viol, budgetFiredOnCompliant: viol.budgetFiredOnCompliant or compliant(c) },
    reach' = reach
  }

  /// The close-out is cut short because the transport is already gone or
  /// expired: best-effort sends fail instantly, the finish completes.
  action closeOutDeadTransport(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).eg == EgCloseOut,
    or { not(conn(c).live), conn(c).expired },
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), eg: EgDone, coBudget: false, pump: false } }),
    conns' = conns, srv' = srv, viol' = viol, reach' = reach
  }

  /// The client pump exits on its own (client_tx dropped at channel_eof /
  /// channel close / connection teardown / response-task abort).
  action clientPumpSelfExit(c: str, i: int): bool = all {
    sessAt(c, i).exists, sessAt(c, i).pump,
    or {
      chanAt(c, i).clientEof,
      chanAt(c, i).ch == ChClosed,
      not(conn(c).live),
      sessAt(c, i).eg == EgAborted
    },
    chans' = chans.set((c, i), { ...chanAt(c, i), sess: { ...sessAt(c, i), pump: false } }),
    conns' = conns, srv' = srv, viol' = viol,
    reach' = { ...reach,
      muxSiblingQuiesced: reach.muxSiblingQuiesced or
        (sessAt(c, i).proto == PExited and CHANS.exists(j => j != i and chans.get((c, j)).sess.guard))
    }
  }

  /// channel_close from the client for an accepted channel: bookkeeping
  /// decrements, the write half is discarded, the ChannelSession entry is
  /// removed — its Drop aborts the response task (guard released if still
  /// held; an in-flight close-out is cut short). The proto task is never
  /// aborted; it exits via its own observation actions.
  action peerChannelClose(c: str, i: int): bool = all {
    chanAt(c, i).ch == ChAccepted,
    conn(c).live, not(conn(c).expired), not(conn(c).herr), not(conn(c).discQ),
    val s = sessAt(c, i)
    val guardWasHeld = s.exists and s.guard
    all {
      chans' = chans.set((c, i), { ...chanAt(c, i),
        ch: ChClosed,
        writeHalf: false,
        sess:
          if (not(s.exists)) s
          else { ...s,
            guard: false,
            eg: if (s.eg == EgFwd or s.eg == EgSendPend or s.eg == EgCloseOut) EgAborted else s.eg,
            sendBudget: false,
            coBudget: false
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

  // --- 7e. adversary peer-class letters (§2c alphabet) ------------------------

  /// connect-silent: the peer never sends the SSH banner (probe / wedged
  /// client); occupancy-withholding pre-auth, no packets ever.
  action peerSilent(c: str): bool = all {
    c == ADV, LET_OCC,
    conn(c).exists, conn(c).live, not(conn(c).banner),
    conns' = conns.set(c, { ...conn(c), occW: true, sendsPk: false }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// park-in-kex: the peer starts but never completes KEX (initial or
  /// re-key), trickling SSH_MSG_IGNORE so keepalive/inactivity never reap;
  /// the handle queue is not drained and a polite disconnect is
  /// undeliverable while it lasts.
  action peerParkInKex(c: str): bool = all {
    c == ADV, LET_OCC,
    conn(c).exists, conn(c).live, conn(c).banner, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), kex: true, occW: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// The peer completes the parked KEX (legitimate completion is always
  /// available to the adversary too — the §2d obligations-complete rule).
  action peerCompletesKex(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).kex,
    conns' = conns.set(c, { ...conn(c), kex: false }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// Generic occupancy-withholding declaration: authenticated-but-no-exec,
  /// open-without-exec (incl. shell/pty/subsystem requests, H12),
  /// exec-then-no-handshake, idle-between-opcodes,
  /// answer-keepalives-forever. Marks the class so the §2d stage deadlines
  /// may fire; the peer keeps answering keepalives (sendsPk stays true).
  action peerWithholdsProgress(c: str): bool = all {
    c == ADV, LET_OCC,
    conn(c).exists, conn(c).live, not(conn(c).occW),
    conns' = conns.set(c, { ...conn(c), occW: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// stop-draining: the peer stops reading (zero TCP receive window /
  /// parked queue) and goes quiet at the SSH level, but keeps ACKing.
  /// Transport-withholding.
  action peerStopDraining(c: str): bool = all {
    c == ADV, LET_WEDGE,
    conn(c).exists, conn(c).live, conn(c).reading,
    conns' = conns.set(c, { ...conn(c), reading: false, sendsPk: false, trW: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// withhold-window-adjust: the peer stops granting SSH channel window.
  /// Transport-withholding (the GW-8 surface; the budget's enabling class).
  action peerWithholdWindow(c: str): bool = all {
    c == ADV, LET_WEDGE,
    conn(c).exists, conn(c).live, not(conn(c).trW),
    conns' = conns.set(c, { ...conn(c), trW: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// vanish-half-open: the peer disappears without FIN/RST — keepalives
  /// unanswered, nothing ACKed (W5). Transport-withholding.
  action peerVanish(c: str): bool = all {
    c == ADV, LET_HOSTILE,
    conn(c).exists, conn(c).live, conn(c).acks,
    conns' = conns.set(c, { ...conn(c), reading: false, sendsPk: false, acks: false, trW: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// The TCP_USER_TIMEOUT setsockopt failed at accept (W7 degraded-tier
  /// gate; warn-and-continue in the code).
  action sockoptFails(c: str): bool = all {
    c == ADV, LET_SOCKOPT_FAIL,
    conn(c).exists, conn(c).live, conn(c).tcpUtoOk,
    conns' = conns.set(c, { ...conn(c), tcpUtoOk: false }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  // --- 7f. russh queue/park mechanics and connection-level timers -------------

  /// The russh session loop drains one handle-queue message to a reading
  /// peer (only at its select point: not during KEX, not while parked).
  action queueDrainDeliver(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).queue > 0,
    not(conn(c).parked), not(conn(c).kex), conn(c).reading, not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), queue: conn(c).queue - 1 }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// The russh session loop's inline socket write parks against a
  /// non-reading peer (rsWriteParked): no callbacks, no keepalive arm, no
  /// further queue drains until enforcement or teardown. Records whether
  /// the force-close was already armed (the enforcement wake) at park time.
  action queueDrainParks(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).queue > 0,
    not(conn(c).parked), not(conn(c).kex), not(conn(c).reading), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), parked: true, parkWake: conn(c).fcArmed }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// Pre-auth deadline, phase 1 (no SSH transport yet): the select drops
  /// the connection future silently — permit and fd released, the gauge was
  /// never incremented (no auth callback fired). Never fires against a
  /// compliant peer (its banner/auth letters complete first, §2d).
  action firePreAuthPhase1(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).preAuth == PaArmed,
    not(conn(c).banner), not(compliant(c)),
    conns' = conns.set(c, { ...connOnDrop(conn(c)), preAuth: PaFired }),
    chans' = CHKEYS.mapBy(k => if (k._1 == c) chanOnConnDrop(chans.get(k)) else chans.get(k)),
    srv' = { ...srv,
      connPermits: if (conn(c).permit) srv.connPermits - 1 else srv.connPermits,
      connsActive: if (conn(c).counted) srv.connsActive - 1 else srv.connsActive
    },
    viol' = viol,
    reach' = { ...reach, kexParkedReclaimed: reach.kexParkedReclaimed or conn(c).kex }
  }

  /// Pre-auth deadline, phase 2 (transport established, still not
  /// authenticated): polite disconnect queued AND the force-close armed at
  /// the same decision point (S12). Enabled only against an
  /// occupancy-withholding peer (§2d); on an authenticated connection the
  /// deadline is latched off (C19 is the silent no-op).
  action firePreAuthPhase2(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).preAuth == PaArmed,
    conn(c).banner, conn(c).stage != CAuthed, conn(c).stage != CChanOpen,
    not(compliant(c)), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), preAuth: PaFired, discQ: true, fcArmed: true }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// Empty-connection grace, action 1 of 2: the timer task wakes and its
  /// re-check passes (still zero live sessions).
  action graceCheckPass(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).grace == GArmed,
    conn(c).liveSess == 0, not(conn(c).herr), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), grace: GChecked }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// Empty-connection grace, action 2 of 2: polite disconnect queued and
  /// the force-close armed at the same decision point (S12). An admission
  /// that lands between the re-check and this step is the documented
  /// W3/P4 race (the carve-out); the strict no-carve-out probe latches it.
  action graceFire(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).grace == GChecked,
    not(conn(c).herr), not(conn(c).expired),
    conns' = conns.set(c, { ...conn(c), grace: GFired, discQ: true, fcArmed: true }),
    chans' = chans, srv' = srv,
    viol' = viol,
    reach' = { ...reach,
      graceFired: true,
      graceFiredWithLiveSession: reach.graceFiredWithLiveSession or conn(c).liveSess > 0
    }
  }

  /// Force-close enforcement (ConnDeadline + FORCE_CLOSE_SLACK): reads and
  /// writes start failing; a parked write is woken only if its enforcement
  /// wake was registered (force-close armed before the park) — the W7
  /// ordering bit. After expiry no bytes are accepted or delivered (S11).
  action fireForceCloseEnforce(c: str): bool = all {
    conn(c).exists, conn(c).live, conn(c).fcArmed, not(conn(c).expired),
    or { not(conn(c).parked), conn(c).parkWake },
    conns' = conns.set(c, { ...conn(c), expired: true, parked: false }),
    chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// The russh session loop returns and the handler drops (the §2b reap
  /// letters, each guarded by its §2d enabling condition): permit, fd,
  /// gauge slot, channel bookkeeping and any still-held SessionGuards are
  /// released through the normal drop path; proto tasks keep running and
  /// exit via their own observation actions.
  action reapTransportEnd(c: str, letter: ReapLetter): bool = all {
    conn(c).exists, conn(c).live,
    match letter {
      // The peer closes / RSTs (also how a compliant peer answers a queued
      // disconnect). The peer's own action — no enabling condition.
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
        sessPermits: srv.sessPermits - released,
        chansActive: srv.chansActive - released
      },
      viol' = { ...viol,
        timerReapedCompliant: viol.timerReapedCompliant or
          (compliant(c) and (letter == LKeepalive or letter == LTcpUserTimeout or letter == LInactivity))
      },
      reach' = { ...reach,
        kexParkedReclaimed: reach.kexParkedReclaimed or (conn(c).kex and conn(c).stage != CAuthed and conn(c).stage != CChanOpen),
        vanishReclaimedDesigned: reach.vanishReclaimedDesigned or
          (not(conn(c).acks) and (letter == LKeepalive or letter == LTcpUserTimeout)),
        parkedReclaimedByInactivity: reach.parkedReclaimedByInactivity or
          (conn(c).parked and letter == LInactivity)
      }
    }
  }

  // --- 7g. server level: accept-error resilience and the three-stage drain ----

  /// A transient accept error (EMFILE/ENFILE class): classified RetryAfter,
  /// the loop sleeps and retries — the listener never leaves the accepting
  /// state (S18).
  action acceptErrorTransient: bool = all {
    LET_UPSTREAM,
    or { srv.phase == SrvServing, srv.phase == SrvNotServing },
    conns' = conns, chans' = chans, srv' = srv, viol' = viol, reach' = reach
  }

  /// SIGTERM: the drain task flips health to NOT_SERVING and sleeps the
  /// drain grace; the accept loop keeps accepting during stage 1.
  action opSigterm: bool = all {
    LET_DRAIN,
    srv.phase == SrvServing,
    srv' = { ...srv, phase: SrvNotServing },
    conns' = conns, chans' = chans, viol' = viol, reach' = reach
  }

  /// Drain stage 2: the drain grace elapses, serve_shutdown fires, the
  /// accept loop returns. Established connections and sessions are
  /// untouched (P6/S17).
  action srvAcceptStop: bool = all {
    srv.phase == SrvNotServing,
    srv' = { ...srv, phase: SrvAcceptStopped },
    conns' = conns, chans' = chans, viol' = viol, reach' = reach
  }

  /// Drain stage 3: the session-drain window elapses with sessions still
  /// live; sessions_shutdown is cancelled — every channel's child token —
  /// and proto tasks observe it at their next poll.
  action srvSessionDrainTimeout: bool = all {
    srv.phase == SrvAcceptStopped,
    srv' = { ...srv, phase: SrvTokenFired },
    conns' = conns, chans' = chans, viol' = viol, reach' = reach
  }

  /// Process exit: only once every connection has fully wound down and
  /// every proto task has exited (the CANCEL_GRACE hard-exit with
  /// stragglers is wall-clock behavior below this abstraction; SIGKILL is
  /// outside the verified envelope).
  action srvExit: bool = all {
    or { srv.phase == SrvAcceptStopped, srv.phase == SrvTokenFired },
    srv.connsActive == 0,
    srv.connPermits == 0,
    liveProtos == 0,
    srv' = { ...srv, phase: SrvExited },
    conns' = conns, chans' = chans, viol' = viol, reach' = reach
  }

  // --- 8. init ---------------------------------------------------------------

  action init = all {
    conns' = CONNS.mapBy(c => freshConn),
    chans' = CHKEYS.mapBy(k => freshChan),
    srv' = { phase: SrvServing, connsActive: 0, chansActive: 0, connPermits: 0, sessPermits: 0 },
    viol' = noViol,
    reach' = noReach
  }

  // --- 9. step ---------------------------------------------------------------

  action step = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // accept / establishment / channel opens
      acceptConn(c),
      peerBanner(c),
      peerAuthFail(c),
      peerAuthOk(c),
      peerOpenChannel(c, i),
      peerOpenOverBound(c),
      peerOpenNonSession(c),
      peerForgedClose(c),
      // exec admission
      execAdmit(c, i),
      execRejectCap(c, i),
      execGatePanic(c, i),
      // proto-task phases
      peerHandshake(c, i),
      peerOpcode(c, i),
      opcodeToRpcWait(c, i),
      opcodeToEmit(c, i),
      rpcCompletesStore(c, i),
      rpcCompletesBuildSubmit(c, i),
      fireRpcDeadline(c, i),
      buildTerminal(c, i),
      buildStreamErrorNonWire(c, i),
      emitDone(c, i),
      peerClientEof(c, i),
      protoSeesClientEof(c, i),
      peerWireGarbage(c, i),
      protoHandlerError(c, i),
      protoObservesShutdown(c, i),
      protoObservesConnGone(c, i),
      fireOpcodeIdle(c, i),
      fireHandshakeTimeout(c, i),
      cancelLoopDone(c, i),
      protoPanic(c, i),
      // egress / finish / close-out
      pumpSendOk(c, i),
      pumpSendBlocks(c, i),
      peerWindowAdjust(c, i),
      fireSendBudget(c, i),
      pumpExitEof(c, i),
      pumpExitSendFail(c, i),
      closeOutSendStatus(c, i),
      closeOutSendEof(c, i),
      closeOutSendClose(c, i),
      closeOutComplete(c, i),
      fireCloseOutBudget(c, i),
      closeOutDeadTransport(c, i),
      clientPumpSelfExit(c, i),
      peerChannelClose(c, i),
      // adversary class letters
      peerSilent(c),
      peerParkInKex(c),
      peerCompletesKex(c),
      peerWithholdsProgress(c),
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      sockoptFails(c),
      // queue / park / timers / force-close / reaps
      queueDrainDeliver(c),
      queueDrainParks(c),
      firePreAuthPhase1(c),
      firePreAuthPhase2(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      // server level
      acceptErrorTransient,
      opSigterm,
      srvAcceptStop,
      srvSessionDrainTimeout,
      srvExit
    }
  }
}

// ===========================================================================
// Regimes (§2e): one thin instantiation module per check; nix/quint.nix
// wires each as its own exhaustive TLC run in Stage C, plus the witness
// checks. Policy caps are dialed per the §2e structural-ceiling vs
// policy-cap table.
// ===========================================================================

/// Base regime: legitimate letters only (connect/auth/open/exec/handshake/
/// opcodes/close/EOF, mux fan-out, window grants). Both connections behave;
/// no fault letters, no drain. Caps sit at the structural ceilings except
/// MAX_SESSIONS = 2, which the 2-conn × 2-channel product exceeds, so the
/// session-cap exec rejection stays reachable (PC-3).
module gwConnLifecycleBase {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// Cap / auth-reject regime: MAX_CONNECTIONS dialed to 1 so the over-cap
/// permit-None path (reject at the first auth callback) is reachable with
/// two modeled connections (S1 cap clause, GW-12 reachability witness).
module gwConnLifecycleCap {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 1,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// fault-peer-occupancy: silent / KEX-parked / never-auth / no-exec /
/// no-handshake / idle-between-opcodes / answer-keepalives-forever letters
/// on conn A; conn B legitimate. The stage deadlines and the pre-auth /
/// empty grace carry the reclamation story (GW-3 family).
module gwConnLifecycleFaultOccupancy {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = true,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// fault-peer-transport: stop-draining, withhold-window-adjust, forged
/// closes, burst/over-bound opens, non-session opens, vanish-half-open on
/// conn A. The send/close-out budgets, force-close and the designed
/// transport reaps carry the reclamation story (GW-2/4/5/6/7/8, W5).
module gwConnLifecycleFaultTransport {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = true,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// fault-upstream: scheduler/store slow/unavailable (rpc-wait deadline
/// fires), non-Wire build-stream errors, transient accept errors (EMFILE).
/// The F9/GW-16/GW-19 surface plus the strict-S16 (W10) probe regime.
module gwConnLifecycleFaultUpstream {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = true,
    LET_DRAIN = false,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// fault-drain: SIGTERM at any point with all base letters live; the
/// three-stage drain, accept-stop permissiveness and the drain-expiry
/// cancel (GW-13, S17/L4/P6).
module gwConnLifecycleFaultDrain {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = false,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = true,
    LET_PANIC = false,
    LET_SOCKOPT_FAIL = false,
    INACTIVITY_AVAILABLE = false
  ).*
}

/// fault-degraded: handler panics, the exec-gate panic splitter, the
/// TCP_USER_TIMEOUT setsockopt failure, plus the park-producing wedge
/// letters (without them no write can park and the W7 degraded tier is
/// unreachable) and — in this regime only — the russh inactivity reap
/// letter. The strict no-inactivity variant of L1 is the pre-registered
/// expected falsification here (the ~1 h degraded tier).
module gwConnLifecycleFaultDegraded {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = true,
    LET_SOCKOPT_FAIL = true,
    INACTIVITY_AVAILABLE = true
  ).*
}
