// -*- mode: Bluespec; -*-
//
// Phase-0 Stage-B model of the rio-gateway connection/channel/session/server
// lifecycle (campaign gw-session-formal, design gw-session-formal-design.md
// DRAFT v2 §2-§3). One transport (the SSH listener), sessions as compound
// nodes, the data plane abstracted to occupancy states. The invariant ↔
// spec-rule linkage lives in the per-predicate Rule(s)/Carrier doc tags
// (section 6 below); check wiring and r[verify] markers live in
// nix/quint.nix, never here.
//
// ===========================================================================
// CAMPAIGN CLOSE-OUT BANNER (gw-session-formal, Track B round 2; CLOSED at
// Phase 0 on the design's "stop after Phase 0" arm, verify-only, 2026-05-30).
// Relocated from the retired gw-session invariant map; this banner is the
// campaign-level record (the closureEvidence archived-banner precedent). The
// full original map text is in git history.
//
// THE VERDICT: the as-built gateway connection/channel/session/server
// lifecycle is correct at the model's resolution — ZERO as-built defects,
// zero code changes needed. Every §3 property (20 safety, 6 permissiveness,
// 8 settlement) holds exhaustively over every wired per-family scope; the
// only reachable violations are the four pre-registered documented windows
// (W3/P4 admit race, W10 orphan bound, P12 panic carve-out, W7 degraded
// tier), each falsifying exactly as predicted and each accepted with a
// recorded bound; no unlisted falsification surfaced in any exhaustive run,
// witness search, or calibration baseline; and the calibration record (the
// gw-f*.qnt VERDICT blocks + the calibration/README.md acceptance section)
// proves the invariant set re-finds all 17 encodable historical bug classes
// (plus the three permissiveness halves) when their fixes are reverted.
//
// OWNER DECISIONS CLOSING THE CAMPAIGN (2026-05-29 design checkpoint, seven
// Track-B decisions): B1 guard-held session accounting (S3/S4 stated against
// guard-held; the W2 divergence is a stated bound, not a defect); B2
// panics-in-envelope / SIGKILL-out; B3 scheduler-side cancel as a named
// assumption; B4 conn-permit-at-accept as a fixed fact (exposure is the
// GW-D4 alerting note); B5 GW-14/15/17/21 test-only; B6 corpus exclusions
// ratified; B7 exit-path STDERR writes left unfixed (below the model's
// abstraction). The 2026-05-30 Phase-0 checkpoint ratified the W2/W5/W7/W10
// window dispositions with their recorded bounds and closed the campaign
// verify-only: the W10 decision rule did not trigger (w10TriggerAbsent holds
// over fam-upstream's 3.25 M states), the optional W7 hardening and
// errors_total{setsockopt} label were not commissioned, the 1-3 small-fix
// budget went unspent, and the two W10 pinning tests parked on the Phase-1
// list land with no Phase 1 — they do not exist; W10's coverage is the wired
// falsification probe plus the scheduler orphan-watcher assumption.
//
// COUNTER-SIGNATURES (close-out verification, workflow run wf_96638bed-4fb,
// 2026-05-30; three independent verification slices re-derived this record's
// claims from the artifacts — rebuilding checks, reading transcripts and
// every cited test/marker site):
//   - checks-vs-records (41 rows: every wired check vs the Stage-C and
//     calibration tables — inventory, module/invariant/step/heap params,
//     verdicts, state counts, costs; spot force-rebuilds): COUNTER-SIGNED;
//     3 discrepancies (2 minor, 1 cosmetic) — measurement-snapshot variance
//     and a rounding overstatement; no false claims.
//   - acceptance-vs-corpus (18 rows: all 14 family rows + the owner-decision
//     rows vs the corpus, all 9 override files, the named test coverage and
//     the decision log): COUNTER-SIGNED-WITH-CORRECTIONS; 4 discrepancies
//     (1 material, 1 minor, 2 cosmetic).
//   - spec-markers-vs-reality (11 rows: the 7 campaign tracey rules, ~40
//     marker sites read against cited code/tests, + the 4
//     environment-assumption rows and wiring/validation claims):
//     COUNTER-SIGNED-WITH-CORRECTIONS; 3 discrepancies (1 minor, 2
//     cosmetic).
// Aggregate: 70 rows, 10 discrepancies — 1 material (corrected in place), 4
// minor, 5 cosmetic; no discrepancy affects any verdict. Spot-rebuild
// evidence: 10 of the 40 checks force-rebuilt on a 192-core host across all
// 5 kinds; exhaustive distinct-state counts reproduce EXACTLY (fam-preauth
// 1,183 / fam-hostile 968,151 / fam-upstream 3,253,999 / fam-wedge 66,059);
// witnesses, probes and calibration guards violate as recorded; the unwired
// gwCalibF3AsBuilt baseline re-run HOLDS at exactly 1,183 distinct states
// (the famPreauth structural cross-check).
//
// CORRECTIONS APPLIED AT CLOSE-OUT (record text only; code, model and
// checks needed no changes): (1) MATERIAL — churn-pin provenance: the
// inventory was mined at cccb4d778 but the corpus at origin/main dfe9a5569;
// the four round-1 LogService cutover commits (1,313 insertions / 112
// deletions across 14 rio-gateway files) sit between the two HEADs; none is
// a lifecycle FIX/HARDENING commit, so no family, candidate or verdict is
// affected. (2) minor — F8 coverage attribution corrected to the
// wire_opcodes upload tests (opcodes_write.rs, incl. the fac58554b
// regression test) + functional/nar_roundtrip.rs (the golden-conformance
// suite has no upload tests). (3) minor — Stage-A commit hashes recorded
// pre-rebase; on the integrated branch they are fd92fb873 / a9f3ee2bc /
// a99314b20 / 56fc66ea4. (4) cosmetic — core-module line count ~2.0k;
// ForceClose::arm_within lives in server/mod.rs, its arm-assertion tests in
// connection.rs. NOTED, NOT CORRECTED: first-violation state counts and 4
// of 13 measured falsification depths are TLC-worker-count and
// parallel-BFS-race dependent (re-runs find equally valid counterexamples
// at 2-5x different counts); rerun comparisons must use verdicts and
// exhaustive counts, not first-violation statistics. The tracey impl list
// for gw.conn.cancel-on-disconnect+3 carries one verb-less cross-reference
// comment site (pre-existing); coverage is not overstated.
//
// ACCEPTED RESIDUALS (the GO's residue, ratified 2026-05-30):
//   - W2 guard-vs-proto-task divergence: channels_active under-counts
//     winding-down sessions <= ~30 s per session; structurally bounded.
//     Pinned by witness-server-side-release; operator note GW-D2.
//   - W5 half-open vanish occupancy: a vanished peer pins one conn permit
//     (of 1000), one session permit (of 4096), the gauge slots, ~512 KiB of
//     duplex buffers, possibly a NAR assembly buffer (<= 4 GiB,
//     MAX_NAR_SIZE), and in-flight upstream work for ~300-330 s until the
//     designed transport reap. Pinned by witness-vanish-reclaimed; GW-D3.
//   - W7 degraded-tier reclamation: with TCP_USER_TIMEOUT setsockopt
//     failed, parked-write reclamation degrades from ~305 s to the ~1 h
//     inactivity backstop; the in-process-unbounded sliver (arming after
//     both russh wakes are spent) is out of model. The gating premise (the
//     setsockopt cannot fail on Linux/EKS) is deployment observable GW-D1.
//     Pinned by falsification-l1-no-inactivity.
//   - W10 non-Wire removal orphan: a non-Wire stream failure with a dead or
//     absent client orphans a running build for up to ~300 s until the
//     scheduler orphan watcher cancels it; the Phase-1 removal-predicate
//     fix was NOT triggered (w10TriggerAbsent holds exhaustively). Pinned
//     by falsification-s16-terminal-only.
//   - Plus the two windows by design: the W3/P4 admit-vs-grace race
//     (bounded by one grace cycle; pinned by l5-no-carve-out) and the P12
//     handler-panic carve-out (pinned by s10-panic; backstopped by the
//     orphan watcher).
//   - Deferred full-regime exhaustive coverage: cross-family interleavings,
//     conn-B concurrent full sessions, two concurrent egress/panic sessions
//     on one connection, and the base-regime "everything legitimate at
//     once" proof remain witness/named-run-pinned only (manual targets).
//   - Out-of-model families on test-only coverage (B5/B6 ratified): F8
//     (GW-14/15 upload bounding — wire_opcodes upload tests +
//     functional/nar_roundtrip.rs; store half owned by the store campaign),
//     F11 (GW-17 credential freshness — session_jwt refresh + I-129 tests),
//     F14 (GW-21 ingress memory budget — drv_cache cap tests; named Phase-2
//     kani candidate), F12/F13 (out of corpus).
//   - Environment assumptions imported, not verified (a regression behind
//     one is NOT caught by this campaign's checks) — the IMPORTED
//     ASSUMPTIONS register below.
//
// WHAT THE CAMPAIGN DOES NOT CLAIM: no deployment, soak or canary
// validation exists (no cluster was ever involved); every figure is a
// model, test or CI measurement. Exhaustive verdicts hold at the §2e bounds
// (2 conns, 2 channel slots on conn A, queue depth 2, window credits 2,
// residue ceiling 2, <=1 tracked build) under restricted per-family
// alphabets; full-alphabet regimes are witness/probe-pinned, not exhausted.
// Nothing is machine-proved at the code level (no kani harness exists for
// any gateway path); the model-code coupling is the calibration corpus plus
// the tracey marker discipline, not refinement. SIGKILL, the data plane
// (upload framing, NAR streaming, STDERR), credential freshness, and the
// adjacent state machines are outside the verified envelope.
//
// IMPORTED ASSUMPTIONS REGISTER (the model imports these rather than
// verifying them; each is owned by another campaign or existing tests):
//   - Scheduler orphan-watcher backstop: a build whose gateway-side watcher
//     is gone (skipped cancel on a handler panic, SIGKILL, or the non-wire
//     removal heuristic guessing wrong) is auto-cancelled by the scheduler
//     once it has no attached watcher (sched.backstop.orphan-watcher;
//     rio-scheduler actor/housekeeping.rs 300 s sweep). Relied on by: S10
//     (panic-regime weakening), S16 / the W10 disposition, the GW-D5
//     SIGKILL note. Owner: the scheduler campaign.
//   - WatchBuild reconnect residual: the in-opcode build-event-wait state
//     has no deadline by design; the model assumes the WatchBuild reconnect
//     loop is finitely bounded (MAX_RECONNECT = 10, capped backoff ~111 s).
//     C4 update (2026-05-31): the resumability layer (since_sequence
//     cursor, event-log replay, dedup) was deleted in favor of
//     snapshot-first attach — the unverified surface shrank to the
//     reconnect loop + snapshot consumption; the boundedness assumption is
//     unchanged. Coverage: test_build_paths_reconnect_exhausted_returns_
//     failure + the snapshot-resync tests (wire_opcodes/build.rs). L6 is
//     scoped to rpc-wait only because of this.
//   - Scheduler stream silent forever with a live client: an accepted build
//     whose BuildEvent stream stays open and silent parks the session in
//     build-event-wait until a terminal event, a transport-reap letter, or
//     shutdown (process_build_events has no timeout arm) — a design
//     assumption; relied on by the §2a abstraction-loss table and the W5
//     occupancy bound.
//   - ForceClose earliest-deadline-wins arithmetic: the numeric "earliest
//     wins / never moves later" half of S11 has no observable negation in a
//     tick-free model; coverage is the fetch_min implementation
//     (ForceClose::arm_within, server/mod.rs) + the arm assertions in the
//     stalled-send / window-starved unit tests (server/connection.rs).
// ===========================================================================
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

  /// Structural channel slots available to the legitimate connection cB
  /// (conn cA always has both). 2 = the §2e default; 1 is the §2e
  /// pre-registered state-space fallback (2) — "drop conn B's channel count
  /// to 1 in that regime" — kept as a constant so a regime can apply the
  /// fallback without touching the transition relation.
  // MODEL-DIVERGENCE(law=gw.conn.channel-limit+4; tree=docs/spec/models/gwConnLifecycle.qnt:regime instantiations): B_CHANNEL_SLOTS is a model constant absent from the design's §2e constants table, added purely to make fallback (2) a per-regime instantiation choice instead of a model edit — retarget-by: the design's constants table absorbing the fallback knob.
  const B_CHANNEL_SLOTS: int

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
  /// Rule(s): gw.conn.cap, gw.conn.real-connection-marker. Carrier:
  /// famPreauth + famCap (over-cap clause).
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
  /// Rule(s): gw.conn.session-cap+2. Carrier: all (global arithmetic);
  /// content-deepest famAdmission/famUpstream.
  val s2SessionPermitConservation = and {
    srv.sessPermits == heldGuards,
    srv.sessPermits <= MAX_SESSIONS
  }

  /// S3 GaugeAccuracy: connections_active counts connections that reached
  /// an auth callback and have not dropped; channels_active equals live
  /// SessionGuards (the guard-held definition of "session").
  /// Rule(s): gw.conn.real-connection-marker, gw.conn.session-cap+2. Carrier:
  /// all; content-deepest famAdmission/famUpstream.
  val s3GaugeAccuracy = and {
    srv.connsActive == CONNS.filter(c => conn(c).exists and conn(c).counted and conn(c).live).size(),
    srv.chansActive == heldGuards
  }

  /// S4 PerConnLiveCount: each connection's live_sessions equals its live
  /// SessionGuards, up to the increment leaked by the exec-gate panic
  /// splitter (the documented fault-letter weakening).
  /// Rule(s): gw.conn.exit-status+3 (grace/live-count pairing). Carrier:
  /// famAdmission (+ panic term famDegraded).
  val s4PerConnLiveCount =
    CONNS.forall(c => conn(c).liveSess == connGuards(c) + (if (conn(c).live) conn(c).panicLeaks else 0))

  /// S5 NoSessionOutlivesConnection (capacity half): when a connection's
  /// transport has ended, none of its sessions still holds a SessionGuard;
  /// the proto tasks' own exits (after at most the cancel obligation) are
  /// armed by construction and pinned by the teardown witnesses.
  /// Rule(s): gw.conn.cancel-on-disconnect+3, gw.conn.lifecycle. Carrier: all
  /// teardown families (famWedge/famUpstream/famDrain).
  val s5NoSessionOutlivesConnection =
    CONNS.forall(c => (conn(c).exists and not(conn(c).live)) implies CHANS.forall(i => not(sessAt(c, i).guard)))

  /// S6 SingleExecPerChannel: a channel's write half is consumed by at most
  /// one admitted exec, ever.
  /// Rule(s): gw.conn.exec-request, gw.conn.per-channel-state. Carrier:
  /// famAdmission/famHostile.
  val s6SingleExecPerChannel = CHKEYS.forall(k => chans.get(k).execs <= 1)

  /// S7 ChannelAccounting: open_channels equals the accepted-and-open set,
  /// stays within the per-connection bound, and execs are admitted only on
  /// accepted, currently-open channels.
  /// Rule(s): gw.conn.channel-limit+4, gw.conn.per-channel-state. Carrier:
  /// famHostile.
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
  /// Rule(s): gw.conn.exec-request. Carrier: famDegraded (panic splitter).
  val s8RejectReleasesCapacity = not(viol.permitStranded)

  /// S9 SingleRelease: no permit, gauge slot, live-count or map entry is
  /// released twice — no negative counters, no double-release latch.
  /// Rule(s): gw.conn.session-cap+2, gw.conn.cap. Carrier: all (shares S2/S3
  /// content).
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
  /// Rule(s): gw.conn.cancel-on-disconnect+3. Carrier: famUpstream/famDrain
  /// (panic carve-out famDegraded).
  val s10CancelOnSessionEnd =
    CHKEYS.forall(k =>
      (sessOf(k).exists and sessOf(k).proto == PExited and sessOf(k).exitR != RPanic and sessOf(k).tracked)
        implies sessOf(k).cancelSent)

  /// S11 ForceCloseArmedSticky: an armed force-close is never cleared, and
  /// after expiry no further bytes are accepted from or delivered to the
  /// peer (latches; the as-built guards keep them false). Expiry only ever
  /// follows arming. The numeric earliest-deadline-wins half lives in the
  /// ForceClose fetch_min unit tests, not the model.
  /// Rule(s): gw.conn.force-close. Carrier: famPreauth (grace/pre-auth arms)
  /// + famWedge (stall arm) + famDegraded (park ordering).
  val s11ForceCloseArmedSticky = and {
    not(viol.fcDisarmed),
    not(viol.deliveredAfterExpiry),
    CONNS.forall(c => conn(c).expired implies conn(c).fcArmed)
  }

  /// S12 DecideImpliesArmed: whenever a polite disconnect is queued
  /// (auth-timeout, empty-grace), the force-close is armed at the same
  /// decision point.
  /// Rule(s): gw.conn.force-close. Carrier: famPreauth + famWedge +
  /// famDegraded (with S11).
  val s12DecideImpliesArmed = CONNS.forall(c => conn(c).discQ implies conn(c).fcArmed)

  /// S13 WindowPacedEgress: data handed to russh never exceeds the
  /// client-granted window plus the bounded handle-queue slot — the
  /// unbounded pendingData path is never used and the queue stays within
  /// its depth, so egress buffering is bounded independent of response
  /// size.
  /// Rule(s): gw.conn.session-cap+2 (window-pacing clause). Carrier:
  /// famWedge.
  val s13WindowPacedEgress = and {
    CHKEYS.forall(k => chans.get(k).pending == 0),
    CONNS.forall(c => conn(c).queue <= QUEUE_DEPTH)
  }

  /// S14 RusshResidueBounded: hidden russh channel-table residue stays
  /// within the policy bound — any open that would grow it past the bound
  /// (over-bound or non-session) terminates the connection instead.
  /// Rule(s): gw.conn.channel-limit+4, gw.conn.channel-types. Carrier:
  /// famHostile.
  val s14RusshResidueBounded = CONNS.forall(c => conn(c).residue <= RESIDUE_BOUND)

  /// S15 ReleaseBeforeCloseOut: capacity release (guard drop) precedes the
  /// close-out sends — a session in its close-out phase never still holds
  /// its guard, so release never depends on the peer accepting the
  /// close-out or on the handle queue draining.
  /// Rule(s): gw.conn.send-deadline, gw.conn.exit-status+3. Carrier:
  /// famWedge/famUpstream.
  val s15ReleaseBeforeCloseOut =
    CHKEYS.forall(k => (sessOf(k).exists and sessOf(k).eg == EgCloseOut) implies not(sessOf(k).guard))

  /// S16 TrackedBuildPolicy (as shipped): a build leaves the tracked set
  /// only when a terminal outcome was processed or the stream error class
  /// was non-Wire (the documented trade-off); never merely because the
  /// session is ending. The strict variant in §6c is the W10 probe.
  /// Rule(s): gw.conn.cancel-on-disconnect+3 (tracking-policy clause).
  /// Carrier: famUpstream.
  val s16TrackedBuildPolicy =
    CHKEYS.forall(k => (sessOf(k).everTracked and not(sessOf(k).tracked))
      implies (sessOf(k).terminal or sessOf(k).nonWireErr))

  /// S17 DrainStageOrder: the shutdown stages advance only in order
  /// (NOT_SERVING ≺ accept-stop ≺ sessions-done/expiry ≺ cancel ≺ exit)
  /// and accept-stop terminates no established connection or session.
  /// Rule(s): gw.drain.three-stage, gw.conn.session-drain. Carrier: famDrain.
  val s17DrainStageOrder = and {
    not(viol.drainOrderViolated),
    not(viol.acceptStopKilled)
  }

  /// S18 ListenerSurvivesAcceptErrors: transient accept errors (EMFILE
  /// class) never stop the accept loop or the process.
  /// Rule(s): gw.conn.accept-resilience. Carrier: famUpstream (EMFILE letter;
  /// latch-only — the as-built letter is a no-op by construction).
  val s18ListenerSurvivesAcceptErrors = not(viol.listenerDied)

  /// S19 CloseOutOrder: the close-out attempts exit-status before eof
  /// before close; delivery is best-effort and bounded by the close-out
  /// budget (S15/L2 carry the budget half).
  /// Rule(s): gw.conn.exit-status+3. Carrier: famWedge/famUpstream.
  val s19CloseOutOrder = CHKEYS.forall(k => and {
    sessOf(k).coEof implies sessOf(k).coStatus,
    sessOf(k).coClose implies sessOf(k).coEof
  })

  /// S20 GraceArmedExactlyWhenEmpty: the grace is armed only on an
  /// authenticated connection with zero live sessions (channel opens are
  /// not activity), and a live healthy authenticated connection with zero
  /// live sessions whose grace has not fired has it armed (or mid-check).
  /// Admission disarms it.
  /// Rule(s): gw.conn.exit-status+3 (grace clause). Carrier:
  /// famPreauth/famAdmission.
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
  /// Rule(s): gw.conn.session-cap+2 (exec-only rejection). Carrier:
  /// famAdmission.
  val p1MuxFanOutAdmitted = not(viol.execRefusedBelowCap)

  /// P2 SurvivesSessionCountZero: a connection whose live-session count
  /// touches zero is not disconnected before the grace fires.
  /// Rule(s): gw.conn.exit-status+3 (grace clause). Carrier: famAdmission.
  val p2SurvivesSessionCountZero = not(viol.disconnectOnTouchZero)

  /// P3 CapRejectsExecOnly: capacity exhaustion is signaled by exec
  /// rejection (channel_failure + exit-status) only — never by refusing the
  /// channel open, never by terminating the connection.
  /// Rule(s): gw.conn.session-cap+2. Carrier: famHostile.
  val p3CapRejectsExecOnly = not(viol.capRefusedOpenOrConn)

  /// P4 WithinGraceExecSurvives: a connection that admits an exec before
  /// the grace re-check passes is not disconnected by that grace cycle (the
  /// post-check admit window is the documented W3 race, carved out — see
  /// the L5-strict probe).
  /// Rule(s): gw.conn.exit-status+3 (grace clause). Carrier: famAdmission.
  val p4WithinGraceExecSurvives = not(viol.gracePreCheckAdmitKilled)

  /// P5 CompliantPeerNotForceClosed: against a connection whose peer has
  /// played only legitimate letters, no send/close-out budget fires, no
  /// keepalive / TCP_USER_TIMEOUT / inactivity reap fires, no force-close
  /// is armed by a budget site, and no well-formed exec is rejected below
  /// the global cap. (The empty grace may still fire on an idle compliant
  /// connection — that is the as-built design, not a wedge response.)
  /// Rule(s): gw.conn.force-close, gw.conn.send-deadline,
  /// gw.conn.keepalive+2. Carrier: famPreauth/famWedge (+ the compliant named
  /// run).
  val p5CompliantPeerNotForceClosed = and {
    not(viol.budgetFiredOnCompliant),
    not(viol.timerReapedCompliant),
    not(viol.execRefusedBelowCap),
    CONNS.forall(c => (conn(c).exists and compliant(c)) implies not(conn(c).fcByBudget))
  }

  /// P6 EstablishedSurviveAcceptStop: stopping the accept loop (drain stage
  /// 2) terminates no established connection or session before the drain
  /// window elapses.
  /// Rule(s): gw.drain.three-stage. Carrier: famDrain.
  val p6EstablishedSurviveAcceptStop = not(viol.acceptStopKilled)

  // -- 6a.4 settlement (L1-L8): armed-style state invariants --

  /// L1 ConnReclaimArmed: every connection holding capacity (fd, permit,
  /// gauge slot, channel state) with no live admitted session has a
  /// designed reclamation path armed/enabled for its current stage; the
  /// inactivity backstop is acceptable only where the regime admits the
  /// letter (fault-degraded — the W7 degraded tier).
  /// Rule(s): gw.conn.force-close, gw.conn.keepalive+2, gw.conn.exit-
  /// status+3. Carrier: famPreauth (occupancy) + famWedge (transport) +
  /// famDegraded (inactivity tier).
  // MODEL-DIVERGENCE(law=gw.conn.force-close; tree=rio-gateway/src/server/connection.rs:reclamation paths): L1's armed-path semantics — stage deadlines and graces count when ARMED (their fire guards protect permissiveness); reap letters and force-close enforcement count when ENABLED; the empty grace re-arms at guard release (S20), which keeps L1 designed-only outside the degraded regime — retarget-by: a change to which reclamation paths exist or their arming discipline.
  val l1ConnReclaimArmed =
    CONNS.forall(c => (conn(c).exists and conn(c).live and conn(c).liveSess == 0)
      implies reclaimPathArmed(c, true))

  /// L2 SendSettleArmed: every state where a session's output has ended, or
  /// a send / close-out is pending toward a transport-withholding peer, has
  /// the capacity already released or a send/close-out budget, an
  /// enforceable force-close, or the pump's own exit armed for it — release
  /// never requires a further peer letter (the wedge response).
  /// Rule(s): gw.conn.send-deadline. Carrier: famWedge.
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
  /// Rule(s): gw.handshake.timeout, gw.conn.lifecycle. Carrier: famAdmission.
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
  /// Rule(s): gw.drain.three-stage, gw.conn.session-drain. Carrier: famDrain.
  // MODEL-DIVERGENCE(law=gw.conn.session-drain; tree=rio-gateway/src/main.rs:drain stages): L4 is encoded as the exit-quiescence clause; the per-stage "shutdown path enabled" obligations hold by construction in this encoding (the fired token enables every proto's observe action) and are pinned by the drain named run — retarget-by: a drain-stage change that decouples the fired token from the proto observe actions.
  val l4DrainObligationsArmed =
    (srv.phase == SrvExited) implies and {
      liveProtos == 0,
      srv.connsActive == 0,
      srv.connPermits == 0
    }

  /// L5 GraceNeverKillsLiveSession: the grace disconnect is never taken in
  /// a state with a live SessionGuard outside the documented post-check
  /// admit race (the carve-out); the un-carved form is the §6c probe.
  /// Rule(s): gw.conn.exit-status+3 (grace clause). Carrier: famAdmission.
  val l5GraceNeverKillsLiveSession = not(viol.graceKilledLiveSession)

  /// L6 RpcWaitDeadlineArmed: every session in in-opcode-rpc-wait has that
  /// wait's upstream deadline armed (every unary/store RPC await carries a
  /// deadline — the F9 surface). Deliberately NOT claimed for
  /// in-opcode-build-event-wait, which has no deadline by design.
  /// Rule(s): gw.conn.cancel-on-disconnect+3 (timeout-wrapped upstream
  /// calls), gw.store.transient-retry. Carrier: famUpstream.
  val l6RpcWaitDeadlineArmed =
    CHKEYS.forall(k => (sessOf(k).exists and sessOf(k).proto == POpRpc) implies sessOf(k).rpcDl)

  /// L7 UpstreamStreamReleasedOnExit: once a session's proto-phase has
  /// exited and its finish/abort has run, the per-build upstream-stream
  /// bit is cleared — the obligation is discharged by session exit itself,
  /// with no peer letter (the W10 decision-rule observable).
  /// Rule(s): gw.conn.cancel-on-disconnect+3. Carrier: famUpstream.
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
  /// Rule(s): gw.conn.lifecycle, gw.conn.per-channel-state. Carrier:
  /// famUpstream/famWedge.
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

  /// The §4 W10 decision-rule trigger, stated as an invariant ("the trigger
  /// never fires"): no reachable state has a build that left the tracked
  /// set with no processed terminal outcome, no CancelBuild attempt, and
  /// the upstream-stream-held bit still set after session exit
  /// (equivalently: an L7 falsification restricted to the BuildEvent
  /// stream). Expected to HOLD — the stream is a handler-local, so every
  /// removal-without-cancel path coincides with the stream dropping — and
  /// the Stage-C upstream-family exhaustive check asserts it; the owner's
  /// W10 sign-off consumes this plus the s16StrictTerminalOnly trace.
  val w10TriggerAbsent =
    CHKEYS.forall(k =>
      (sessOf(k).everTracked and not(sessOf(k).tracked) and not(sessOf(k).terminal)
        and not(sessOf(k).cancelSent) and sessOf(k).proto == PExited)
      implies not(sessOf(k).streamHeld))

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
  // MODEL-DIVERGENCE(law=gw.conn.channel-limit+4; tree=rio-gateway/src/server/connection.rs:channel table): channel slots are use-once (a closed slot is not reused); over-bound and non-session open attempts need no free slot, so the §2e attempt letters stay enabled at the structural ceiling — retarget-by: a channel-table change that reuses slot identities within a connection's lifetime.
  action peerOpenChannel(c: str, i: int): bool = all {
    conn(c).exists, conn(c).live, not(conn(c).kex), not(conn(c).herr), not(conn(c).expired), not(conn(c).discQ),
    conn(c).stage == CAuthed or conn(c).stage == CChanOpen,
    chanAt(c, i).ch == ChNone,
    c == ADV or i < B_CHANNEL_SLOTS,
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
  // MODEL-DIVERGENCE(law=gw.conn.exec-request; tree=rio-gateway/src/session.rs:exec gate): the exec-gate panic splitter lands after session_admitted (live-count incremented, grace disarmed, write half taken) and before the gauge bump/guard creation — the only fallible/awaitable site in the gate per the W1/W2 review evidence — so S3 stays exact and S4 carries the documented panic term — retarget-by: an exec-gate change that adds a fallible await elsewhere in the gate.
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
  // MODEL-DIVERGENCE(law=gw.handshake.timeout; tree=rio-gateway/src/server/connection.rs:stage deadlines): the stage-deadline fire guards (pre-auth, handshake, opcode-idle) are keyed on "peer not in the compliant class" rather than the §2d occupancy-letter enumeration, so transport-withholding-only peers stay reapable by their stage deadline; the compliant-class exclusion (the load-bearing half) is exact — retarget-by: a §2d enumeration change for the stage-deadline rows.
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
  // MODEL-DIVERGENCE(law=gw.conn.session-cap+2; tree=rio-gateway/src/session.rs:egress pump): egress is decoupled — window credits pace data sends (the GW-8 surface) while the handle queue carries close-out/disconnect deliveries and write-park mechanics (the GW-2/W7 surface); their cross-coupling adds state without property content — retarget-by: an egress change that couples data pacing to the handle queue.
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
  // MODEL-DIVERGENCE(law=gw.conn.exit-status+3; tree=rio-gateway/src/server/connection.rs:empty-connection grace): the empty-grace fire carries no compliant-class exclusion (per the §2d table) — an idle compliant connection is grace-disconnected exactly as in production; P5 is therefore encoded over the budget/reap/cap latches and budget-armed force-close sites, not over the grace — retarget-by: a grace-fire change that exempts compliant peers.
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
  // MODEL-DIVERGENCE(law=gw.conn.force-close; tree=rio-gateway/src/server/connection.rs:ForceClose enforcement): enforcement reaches a parked write only if the force-close was armed when the write parked (the parkWake bit) — the design's enforcement-once-armed letter taken literally makes the W7 degraded tier unreachable; the leftover-wake sliver stays out of model exactly as §2a scopes it — retarget-by: an enforcement-path change that wakes parked writes unconditionally.
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
  // MODEL-DIVERGENCE(law=gw.conn.keepalive+2; tree=rio-gateway/src/server/mod.rs:TCP_USER_TIMEOUT): the kernel reap fires on the peer-class form (vanished peers and transport-withholding peers with an undeliverable write outstanding, never the compliant class), not the §2d table's literal ACK-exclusion — the literal exclusion makes the §4 W7 ~305 s tier unreachable and leaves the transport wedge with no designed reclamation — retarget-by: a re-specification of the TCP_USER_TIMEOUT enabling class in the keepalive/reap rule or the W7 tier.
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
  // MODEL-DIVERGENCE(law=gw.conn.accept-resilience; tree=rio-gateway/src/server/mod.rs:accept loop): S18 has no dedicated reachability witness — the EMFILE letter is a no-op by construction; its content is the listener-death latch plus the GW-19 calibration (gw-f10-accept.qnt) — retarget-by: an accept-loop change that makes transient accept errors observable in connection state.
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
  // MODEL-DIVERGENCE(law=gw.drain.three-stage; tree=rio-gateway/src/main.rs:shutdown): process exit is modeled as enabled only at full quiescence (no counted connections, no live proto tasks); the real CANCEL_GRACE hard-exit with stragglers is wall-clock behavior below the tick-free abstraction (and SIGKILL is outside the envelope per owner decision B2) — retarget-by: a shutdown-path change that exits with counted occupancy by design.
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

  // --- 10. named runs ---------------------------------------------------------
  // Deterministic reproductions of the design's pinned traces. Stage C wires
  // them via mkQuintRunCheck; they also serve as executable documentation of
  // the intended scenario for the corresponding witnesses.

  /// The compliant peer's full lifecycle (design §3.2 P5 named run, base
  /// regime): connect → auth → open → exec → handshake → build opcode →
  /// terminal → close-out in order → channel close → peer close. No budget
  /// fires, no force-close, no timer reap, and every resource is back to
  /// zero at the end.
  run compliantLifecycleRun =
    init
      .then(acceptConn("cB"))
      .then(peerBanner("cB"))
      .then(peerAuthOk("cB"))
      .then(peerOpenChannel("cB", 0))
      .then(execAdmit("cB", 0))
      .then(peerHandshake("cB", 0))
      .then(peerOpcode("cB", 0))
      .then(opcodeToRpcWait("cB", 0))
      .then(rpcCompletesBuildSubmit("cB", 0))
      .then(buildTerminal("cB", 0))
      .then(emitDone("cB", 0))
      .then(peerClientEof("cB", 0))
      .then(protoSeesClientEof("cB", 0))
      .then(pumpExitEof("cB", 0))
      .then(closeOutSendStatus("cB", 0))
      .then(queueDrainDeliver("cB"))
      .then(closeOutSendEof("cB", 0))
      .then(queueDrainDeliver("cB"))
      .then(closeOutSendClose("cB", 0))
      .then(closeOutComplete("cB", 0))
      .then(peerChannelClose("cB", 0))
      .then(reapTransportEnd("cB", LPeerClose))
      .expect(and {
        not(conns.get("cB").fcArmed),
        not(viol.budgetFiredOnCompliant),
        not(viol.timerReapedCompliant),
        reach.closeOutOrdered,
        srv.sessPermits == 0,
        srv.chansActive == 0,
        srv.connsActive == 0,
        srv.connPermits == 0,
        allInvariants
      })

  /// The GW-2 / C14 wedge response (fault-peer-transport regime): a
  /// window-starved send toward a stop-draining peer exhausts the send
  /// budget — capacity is released first, the close-out starts under its
  /// own budget, and the force-close is armed.
  run stallArmsForceCloseRun =
    init
      .then(acceptConn("cA"))
      .then(peerBanner("cA"))
      .then(peerAuthOk("cA"))
      .then(peerOpenChannel("cA", 0))
      .then(execAdmit("cA", 0))
      .then(pumpSendOk("cA", 0))
      .then(pumpSendOk("cA", 0))
      .then(pumpSendBlocks("cA", 0))
      .then(peerStopDraining("cA"))
      .then(fireSendBudget("cA", 0))
      .expect(and {
        reach.stallFcArmed,
        conns.get("cA").fcArmed,
        conns.get("cA").liveSess == 0,
        srv.sessPermits == 0,
        chans.get(("cA", 0)).sess.eg == EgCloseOut,
        allInvariants
      })
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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
    B_CHANNEL_SLOTS = 2,
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

/// Base regime under the §2e pre-registered state-space fallback (2): the
/// legitimate connection cB is limited to one channel slot (cA keeps both,
/// so the mux/blast-radius content stays on the adversary side). This is
/// the measurement variant for the Stage-B budget decision; Stage C decides
/// per regime whether the wired check uses the full or the fallback bound.
module gwConnLifecycleBaseSlim {
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
  ).*
}


// ===========================================================================
// Per-family restricted-alphabet modules (§2e fallbacks (3)/(4), the wired
// exhaustive set decided by the Stage-B measurement milestone).
//
// The Stage-B B-measure showed the full-alphabet regimes sit in the high
// tens-of-millions of distinct states (base: ≈59.7 M distinct at depth 24,
// incomplete at 629 s on a 192-worker host; fallback (2) alone shrinks the
// space only ≈2×) — an order of magnitude past the §2e per-check budget —
// and the Stage-C re-measurement showed the same is true of any restriction
// that keeps a rich connection-level alphabet multiplied by two concurrent
// full session lifecycles. Per the §2e pre-committed fallback order, the
// merge-gated EXHAUSTIVE checks therefore run on these per-family modules.
// Each keeps every §2e structural / environment bound (two connection
// slots, two channel slots on conn A, handle-queue depth 2, window credits
// 2, residue ceiling 2, ≤1 tracked build per session) and its regime's §2e
// policy-cap values, applies fallback (2) (B_CHANNEL_SLOTS = 1), and
// restricts the ALPHABET only (the calibration `calibStep` pattern: a
// `famStep` composed from the core actions):
//   - to one corpus family's letters plus the minimum supporting lifecycle
//     letters (so each check multiplies at most one "rich" dimension —
//     either the connection-level fault surface or the in-session
//     machinery — never both), and
//   - to the §2e role asymmetry taken one step further: conn B (the
//     legitimate connection) plays the connection-level alphabet — accept,
//     banner, auth, its grace, its reaps — so cross-connection
//     permit/gauge/grace arithmetic stays exercised, while the
//     channel/session-level letters are played by conn A (famHostile
//     additionally lets conn B open its channel and take the session-cap
//     exec rejection, the P3 surface).
// No environment bound is reduced below the design's registered floors and
// no §3.4 candidate's required structural slot is removed — the loss
// relative to the full regimes is the cross-family / B-side-session /
// (where noted) second-concurrent-session interleavings only, recorded per
// property in the section-6 Carrier doc tags. The
// full-alphabet regime modules above stay as the home of the witness /
// expect-violation / named-run checks (which stop at first violation) and
// of the Stage-C calibration overrides.
// ===========================================================================

/// Family: pre-auth establishment and occupancy withholding at the
/// connection level (corpus F2/F3 pre-auth half, GW-3/GW-4): the ConnStage
/// machine, accept-time permits, the gauge latch on the first auth callback
/// (accept and reject outcomes), silent / KEX-parked / never-auth peers,
/// the two-phase pre-auth deadline, the empty-connection grace on idle
/// authenticated connections, decide-implies-arm, force-close enforcement
/// and every designed reap letter. No channels or sessions — the admission
/// interplay lives in famAdmission.
module gwConnLifecycleFamPreauth {
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
  ).*

  action famStep = {
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

/// Family: exec admission against the empty-connection grace and the
/// per-session stage deadlines (corpus F2 post-auth half plus the GW-18
/// admission surface): the grace arm/disarm/re-arm cycle vs admission (the
/// documented W3/P4 race window), the handshake and opcode-idle deadlines,
/// occupancy-withholding in-session peers, and re-admission after the
/// session count touches zero — on conn A's two channels with the session
/// egress/finish machinery abstracted to channel-close release (famWedge /
/// famUpstream own the pump and close-out paths). Conn B holds its
/// permit/gauge slot and its own grace.
module gwConnLifecycleFamAdmission {
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      peerWithholdsProgress(c),
      graceCheckPass(c),
      graceFire(c),
      reapTransportEnd(c, letter),
      // admission and session-deadline letters, conn A only
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      fireHandshakeTimeout(ADV, i),
      fireOpcodeIdle(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

/// Cap family: MAX_CONNECTIONS dialed to 1 (the §2e cap-regime value) so
/// the over-cap permit-None path — torn down at the first auth callback —
/// is exhaustively explored with two modeled connections (S1's cap clause,
/// the GW-12 surface). Whichever connection accepts second is the
/// permit-less one; sessions stay on conn A; no fault letters.
module gwConnLifecycleFamCap {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 1,
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      graceCheckPass(c),
      graceFire(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      // session-level letters, conn A only
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerClientEof(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i),
      peerChannelClose(ADV, i)
    }
  }
}

/// Family: egress pacing and the wedge response (corpus F5, GW-2/GW-8; the
/// W5 vanish reap and the stalled-send → force-close arm). Window credits,
/// send/close-out budgets, capacity release before close-out, the close-out
/// ordering, queue parks against non-reading peers, and the designed
/// transport reaps for vanished peers — exhaustive for one egress session
/// on conn A (the wedge candidates are single-session shapes; concurrent
/// multi-session content lives in famUpstream / famHostile and the
/// full-regime witnesses). Conn B is the compliant control for P5.
module gwConnLifecycleFamWedge {
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      // the transport-withholding letters (conn A by their own guards)
      peerStopDraining(c),
      peerWithholdWindow(c),
      peerVanish(c),
      // the single egress session, conn A channel 0
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

/// Family: channel accounting under hostile opens and closes (corpus F7,
/// GW-5/GW-6/GW-7): forged/duplicate closes, over-bound and non-session
/// opens, wire garbage, and the bookkeeping/residue consequences on conn
/// A's two channels; conn B opens its own channel and takes the
/// session-cap exec rejection (the P3 exec-only-rejection surface).
/// Egress pacing, the close-out ladder, occupancy letters and the
/// in-opcode data plane are excluded.
module gwConnLifecycleFamHostile {
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections; conn B may open its
      // channel and have its exec rejected at the session cap (P3)
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      peerOpenChannel(c, i),
      execRejectCap(c, i),
      peerClientEof(c, i),
      peerChannelClose(c, i),
      reapTransportEnd(c, letter),
      // the hostile letters (conn A by their own guards)
      peerOpenOverBound(c),
      peerOpenNonSession(c),
      peerForgedClose(c),
      // session letters, conn A only
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerWireGarbage(ADV, i),
      protoSeesClientEof(ADV, i),
      protoObservesConnGone(ADV, i),
      pumpExitEof(ADV, i),
      pumpExitSendFail(ADV, i),
      clientPumpSelfExit(ADV, i)
    }
  }
}

/// Family: in-opcode occupancy, upstream deadlines, build tracking and the
/// teardown obligations (corpus F6/F9, GW-9/GW-10/GW-16/GW-19; the W10/W2
/// surface): rpc-wait vs build-event-wait, the rpc deadline, terminal /
/// non-Wire stream outcomes, the cancel loop, the close-out ladder and the
/// transient-accept-error letter, on conn A's two channels. The egress
/// window machinery and the occupancy/wedge/grace letters are excluded.
module gwConnLifecycleFamUpstream {
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      reapTransportEnd(c, letter),
      acceptErrorTransient,
      // session / opcode / build / close-out letters, conn A only
      peerOpenChannel(ADV, i),
      execAdmit(ADV, i),
      peerHandshake(ADV, i),
      peerOpcode(ADV, i),
      opcodeToRpcWait(ADV, i),
      opcodeToEmit(ADV, i),
      rpcCompletesStore(ADV, i),
      rpcCompletesBuildSubmit(ADV, i),
      fireRpcDeadline(ADV, i),
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

/// Family: the three-stage drain (corpus F10, GW-13): SIGTERM at any
/// point, accept-stop permissiveness, the session-drain expiry token, the
/// drain-expiry cancel of a tracked build, and exit quiescence. The build
/// path is included so a tracked build can be live at every drain stage;
/// the non-build opcode branch, the close-out ladder, egress windows and
/// the fault letters are excluded.
module gwConnLifecycleFamDrain {
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
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet i = CHANS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      reapTransportEnd(c, letter),
      // the drain stages
      opSigterm,
      srvAcceptStop,
      srvSessionDrainTimeout,
      srvExit,
      // session / build letters, conn A only
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

/// Family: the degraded tier (design §2e fault-degraded): the exec-gate
/// panic splitter and proto-task panics (P12), the TCP_USER_TIMEOUT
/// setsockopt failure, write parks ordered against force-close arming (the
/// W7 ordering bit), and the inactivity reap backstop that exists only in
/// this regime's alphabet — exhaustive for one session on conn A carrying
/// the build-tracking chain (so the panic-leaks-a-tracked-build shape is
/// in scope). The grace machinery, hostile opens and the non-build opcode
/// branch are excluded (famPreauth / famAdmission / famHostile /
/// famUpstream own them).
module gwConnLifecycleFamDegraded {
  import gwConnLifecycle(
    MAX_CONNECTIONS = 2,
    MAX_CHANNELS_PER_CONN = 2,
    MAX_SESSIONS = 2,
    RESIDUE_BOUND = 1,
    B_CHANNEL_SLOTS = 1,
    LET_OCC = false,
    LET_WEDGE = true,
    LET_HOSTILE = false,
    LET_UPSTREAM = false,
    LET_DRAIN = false,
    LET_PANIC = true,
    LET_SOCKOPT_FAIL = true,
    INACTIVITY_AVAILABLE = true
  ).*

  action famStep = {
    nondet c = CONNS.oneOf()
    nondet letter = REAP_LETTERS.oneOf()
    any {
      // connection-level letters, both connections
      acceptConn(c),
      peerBanner(c),
      peerAuthOk(c),
      queueDrainDeliver(c),
      queueDrainParks(c),
      fireForceCloseEnforce(c),
      reapTransportEnd(c, letter),
      // the degraded-tier and wedge letters (conn A by their own guards)
      sockoptFails(c),
      peerStopDraining(c),
      peerWithholdWindow(c),
      // the single session, conn A channel 0: admission, the build chain,
      // panics, egress sends and the close-out ladder
      peerOpenChannel(ADV, 0),
      execAdmit(ADV, 0),
      execGatePanic(ADV, 0),
      peerHandshake(ADV, 0),
      peerOpcode(ADV, 0),
      opcodeToRpcWait(ADV, 0),
      rpcCompletesBuildSubmit(ADV, 0),
      protoPanic(ADV, 0),
      pumpSendOk(ADV, 0),
      pumpSendBlocks(ADV, 0),
      fireSendBudget(ADV, 0),
      peerClientEof(ADV, 0),
      protoSeesClientEof(ADV, 0),
      protoHandlerError(ADV, 0),
      protoObservesConnGone(ADV, 0),
      cancelLoopDone(ADV, 0),
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
