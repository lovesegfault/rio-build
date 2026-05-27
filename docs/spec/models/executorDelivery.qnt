// -*- mode: Bluespec; -*-
//
// Model D of the executor-lifecycle campaign (#1, Phase 0 Stage B): the
// AS-BUILT builder-side delivery choreography — the reconnect loop, the
// permanent sink, the relay swap-after-confirmed-open, the stream-local
// in-flight cell, the half-close flush, the SIGTERM drain gate, the
// idle exit and the generation watermark (P3's builder half + P4's exit
// choreography of executor-formal-design.md §2, at await-point
// granularity). This is exactly the machinery the replacement deletes
// in favour of "retry one unary until acked", which is why it is
// verified rather than assumed. Model D discharges the assumption
// Model S (executorSession.qnt) makes about its peer: a started
// build's report is eventually delivered to some leader unless the pod
// dies first, and is never delivered twice as two different outcomes.
//
// Every action cites the production function it encodes (the 0a-pinned
// anchors are in docs/spec/models/executor-invariant-map.md). Check
// wiring and r[verify] markers live in nix/quint.nix, never here.
//
// WHAT IS MODELED
//   - One builder process: the single BuildSlot (claim → build-terminal
//     → guard release), the completion_pending flag (armed before the
//     first await — bug_012), the permanent sink (≤1 report), the
//     relay's one-message buffer, the relay target watch
//     (parked/swapped), the per-connection gRPC channel as a
//     stream-local in-flight cell DISTINCT from endpoint receipt, the
//     graceful half-close flush (bug_117), the reconnect-drain gate
//     (I-063/I-195/bug_472), the I-116 idle exit, the generation
//     watermark (B5 fetch_max) and the stale-assignment fence.
//   - The stream lifecycle at await-point granularity: Opening
//     (requested, not yet confirmed) is distinct from Open; the relay
//     target is swapped only after the open confirms (swap-after-Ok,
//     merged_bug_020/6b6cfcf10); anything sitting in a stream's
//     in-flight cell when that stream dies is dropped; process exit
//     drops the in-flight cell unless the half-close flush ran.
//   - The scheduler endpoint as environment: it receives what a live
//     stream flushes, its serving generation moves on failover
//     (the watermark must not treat "generation moved" as "holder
//     changed" — no destructive builder-side reaction is modeled
//     because none exists in the code), and it never re-delivers.
//   - SIGTERM (drain transition), SIGKILL (fault-process), stream
//     death and open failure (fault-stream) as environment actions
//     with per-regime budgets.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - Log/banner upload, prefetch, cancel handling, heartbeat content
//     beyond the generation latch: the data plane is LogService's
//     subject; CancelSignal changes the OUTCOME of the one report, not
//     the delivery choreography this model checks.
//   - Multiple builds: the pod is one-shot by design
//     (sched.executor.one-shot); a single assignment/report pair is
//     the full state space of interest.
//   - The scheduler-side processing of the received report (Model S's
//     handle_completion) and the scheduler-side repair when the report
//     is lost with the pod (Model S's disconnect/establishment arms).
//   - Scheduler endpoint acks: WorkAssignmentAck is logged and dropped
//     by the scheduler (no decision reads it), so it is not state.
//
// ASSUME-GUARANTEE CHECKLIST
//   - leaderElection.qnt: staleLeaderHasStaleGeneration is what makes
//     the watermark fence (is_stale_assignment) meaningful — a deposed
//     leader's pushes carry a strictly smaller generation once any
//     newer response was observed; boundedDualLeadership bounds the
//     window in which an old-generation push can still be accepted.
//     The acquire-epoch-vs-generation distinction (a holder change
//     need not move the generation once the PG floor saturates) is
//     imported from the log campaign's record: the model therefore
//     never treats "generation moved" as "holder changed", and the
//     fence is the only consumer of the watermark.
//   - executorSession.qnt (Model S): consumes this model's guarantee
//     (delivery-or-death, at-most-once) as its peer assumption; in
//     return Model S guarantees that a report which reaches a serving
//     leader is processed idempotently (the row-already-terminal
//     discipline) — nothing here depends on HOW the scheduler
//     processes it.
//   - The scheduler endpoint's loss behaviour is environment: a stream
//     the server tears down loses its in-flight cell. That loss shows
//     up scheduler-side as a pod death / bare disconnect, which Model
//     S's repair arms own.
//
// ENCODING DECISIONS
//   - Single state record (`st`) plus a violation-latch record (`viol`)
//     and a reachability record (`reach`), same shape as Model S.
//   - completion_pending is armed when the build task starts (the
//     bug_012 fix arms it before the first await); enqueue-into-sink
//     and slot-guard release are separate, freely interleavable
//     actions, so the panic ordering (release before enqueue) and the
//     normal ordering (enqueue before release) are both explored.
//   - The relay hand-off clears completion_pending on mpsc-send-Ok
//     (into the in-flight cell), exactly as relay_loop does — NOT on
//     endpoint receipt; the gap between the two is what the half-close
//     flush and the drain gate exist to cover, and what the in-flight
//     cell makes explicit.
//   - Stream ids are bounded by STREAM_CEIL as an action precondition;
//     fault budgets are per-regime constants.
//
// The invariants, witnesses, regimes and their check wiring are
// documented at their definitions and in nix/quint.nix. Measured state
// counts, depths and wall-clocks live in the introducing commits'
// messages and the CI checks' transcripts, never in this file.
module executorDelivery {
  // --- 1. types ---------------------------------------------------------

  /// The current BuildExecution attempt's state, as the builder sees it.
  /// DSNone    = no stream requested (start, or after a death/park).
  /// DSOpening = build_execution() awaited, not yet confirmed.
  /// DSOpen    = confirmed open (the response stream arrived).
  /// DSDead    = closed/errored/torn down.
  type StreamSt = DSNone | DSOpening | DSOpen | DSDead

  /// The single BuildSlot.
  type SlotSt = SlotIdle | SlotClaimed | SlotDone

  /// The single WorkAssignment's lifecycle at this builder.
  type AsgSt = AsgNone | AsgInFlight | AsgAccepted | AsgRejected

  type DState = {
    // stream / relay
    streamSt: StreamSt,
    streamId: int,            // id of the current stream attempt (0 = none yet)
    streamsOpened: int,       // ceiling counter
    relayTarget: int,         // 0 = parked, else the stream id the relay pumps into
    // the report's location (at most one of these holds it)
    sink: bool,               // in the permanent sink (sink_rx)
    relayBuf: bool,           // in the relay's one-message buffer
    inFlightStream: int,      // 0 = none, else the stream whose channel buffer holds it
    delivered: int,           // endpoint receipts
    // build / completion state
    slot: SlotSt,
    asg: AsgSt,
    buildDone: bool,          // the build produced its terminal outcome
    completionPending: bool,  // "a report is owed and not yet handed to a confirmed-open stream"
    sinkEverQueued: bool,     // send_completion ran at least once
    // exit choreography
    sigterm: bool,
    draining: bool,
    halfCloseRan: bool,
    exited: bool,
    exitedGracefully: bool,
    // generation fence
    schedGen: int,            // the serving leader's generation (environment)
    watermark: int,           // latest_generation (B5 fetch_max latch)
    asgGen: int,              // the generation the in-flight assignment carries
    // budgets
    openFailsLeft: int,
    streamDeathsLeft: int,
    genMovesLeft: int,
    sigkillsLeft: int
  }

  type Viol = {
    relayedIntoUnconfirmed: bool, // the relay handed the report to a never-confirmed stream
    exitWithReportOwed: bool,     // a graceful exit ran while the report was still owed/unflushed
    staleAssignAccepted: bool,    // an assignment below the watermark was accepted
    reportLostWhileOwed: bool,    // the owed report vanished while the process lived
    secondDelivery: bool          // the endpoint received two reports
  }

  /// Only the one genuinely transient event is stored; the other
  /// witnesses are predicates over the current state.
  type Reach = {
    halfCloseFlushed: bool        // the half-close flush delivered the report
  }

  // --- 2. constants (const = overridable per regime) ---------------------

  /// Max BuildExecution attempts (reconnect ceiling).
  const STREAM_CEIL: int
  /// fault-stream: how many confirmed-open streams may die under the relay.
  const STREAM_DEATHS: int
  /// fault-stream: how many open attempts may fail to confirm.
  const OPEN_FAILS: int
  /// environment: how many times the serving generation may move (failover).
  const GEN_MOVES: int
  /// fault-process: SIGKILL (grace expired / OOM) — exit without choreography.
  const SIGKILLS: int

  // --- 3. pure defs -----------------------------------------------------

  /// runtime/mod.rs::is_stale_assignment — strictly less than the
  /// observed watermark.
  pure def staleAssign(asgGen: int, watermark: int): bool = asgGen < watermark

  /// The report is still owed by the process and has not reached any
  /// stream's channel or the endpoint.
  pure def reportHeldLocally(stx: DState): bool = stx.sink or stx.relayBuf

  // --- 4. state variables -----------------------------------------------

  var st: DState
  var viol: Viol
  var reach: Reach

  // --- 5. invariants ------------------------------------------------------

  /// F2 (builder half) — a terminal outcome queued for delivery is
  /// never silently dropped while the process is alive and the report
  /// is still owed: it stays in the sink or the relay buffer until it
  /// is handed to a CONFIRMED-open stream (the hand-off is what clears
  /// completion_pending). Stream churn, failed opens, parked relays
  /// and relay swaps never lose it.
  val reportSurvivesStreamChurn = and {
    not(viol.reportLostWhileOwed),
    // while owed and produced and not yet handed over, it is held locally
    or {
      st.exited,
      not(st.completionPending),
      not(st.sinkEverQueued),
      reportHeldLocally(st)
    }
  }

  /// The relay hands the report only to a stream whose open was
  /// confirmed (swap-after-Ok, merged_bug_020); a never-confirmed
  /// stream can never hold the in-flight cell.
  val relayOnlyIntoConfirmed = not(viol.relayedIntoUnconfirmed)

  /// F2 (builder half) — no graceful exit path runs while a completion
  /// is owed and not yet flushed to a confirmed-open stream
  /// (reconnect_drain_gate + wait_build_flushed + the half-close).
  val noExitWithReportOwed = not(viol.exitWithReportOwed)

  /// The endpoint never receives the report twice (the sink holds at
  /// most one report and the relay moves it at most once) — the
  /// at-most-once half of exactly-once-or-death; the "or death" half
  /// is Model S's repair obligation.
  val atMostOnceDelivery = and {
    st.delivered <= 1,
    not(viol.secondDelivery)
  }

  /// The B5 generation fence: an assignment carrying a generation
  /// below the observed watermark is never accepted, and the watermark
  /// itself never regresses (fetch_max).
  val staleAssignmentsRejected = and {
    not(viol.staleAssignAccepted),
    st.watermark <= st.schedGen
  }

  /// Ceiling/budget sanity (the ceiling-as-precondition discipline).
  val boundsOK = and {
    st.streamsOpened <= STREAM_CEIL,
    st.openFailsLeft >= 0,
    st.streamDeathsLeft >= 0,
    st.genMovesLeft >= 0,
    st.sigkillsLeft >= 0,
    st.delivered >= 0
  }

  /// All-invariants conjunction for the CI checks.
  val allInvariants = and {
    boundsOK,
    reportSurvivesStreamChurn,
    relayOnlyIntoConfirmed,
    noExitWithReportOwed,
    atMostOnceDelivery,
    staleAssignmentsRejected
  }

  // --- 5b. expect-violation witnesses (non-vacuity probes) ----------------
  // Predicate forms over the current state where the contended
  // condition persists; the flush event keeps its one-bit ghost.

  /// A relay target is swapped in (a confirmed-open reconnect stream)
  /// while the report is still owed.
  val noSwapWithReportOwed =
    not(st.completionPending and st.relayTarget != 0 and st.streamsOpened > 1)
  /// The report left the process (it was queued at some point, it is
  /// in no local holding place, the endpoint never received it) while
  /// the process is still alive — the in-flight cell was dropped by a
  /// stream failure.
  val noInFlightCellDropped =
    not(and {
      not(st.exited),
      st.sinkEverQueued,
      st.delivered == 0,
      not(st.sink), not(st.relayBuf), st.inFlightStream == 0
    })
  val noHalfCloseFlush = not(reach.halfCloseFlushed)
  /// The drain gate is holding the exit because a report is still owed.
  val noExitBlockedWhileOwed =
    not(st.draining and st.slot != SlotClaimed and st.completionPending)
  /// The generation fence rejected a stale push.
  val noStaleAssignmentRejected =
    not(st.asg == AsgRejected and st.asgGen < st.watermark)

  // --- 6. actions ----------------------------------------------------------

  /// runtime/mod.rs::run, top of 'reconnect: request a fresh
  /// BuildExecution stream (ExecutorRegister goes out on the new
  /// channel directly). The relay stays parked until the open
  /// confirms.
  action streamOpenStart: bool = all {
    not(st.exited),
    st.streamSt == DSNone or st.streamSt == DSDead,
    st.streamsOpened < STREAM_CEIL,
    // the drain gate's idle fast-path (I-195) breaks BEFORE re-register
    // when draining with nothing owed, so a new attempt implies work
    // remains or draining has not started
    not(st.draining and st.slot != SlotClaimed and not(st.completionPending)),
    st' = { ...st,
      streamSt: DSOpening,
      streamId: st.streamsOpened + 1,
      streamsOpened: st.streamsOpened + 1 },
    viol' = viol,
    reach' = reach
  }

  /// build_execution().await returns Ok: the stream is confirmed open
  /// and ONLY NOW does the reconnect loop swap the relay target onto
  /// it (send_replace(Some(grpc_tx)) — swap-after-Ok, merged_bug_020).
  action streamOpenOk: bool = all {
    st.streamSt == DSOpening,
    st' = { ...st, streamSt: DSOpen, relayTarget: st.streamId },
    viol' = viol,
    reach' = reach
  }

  /// build_execution().await returns Err (leader settling, balance
  /// channel stale): no swap ever happened, the relay stays parked,
  /// the loop backs off and retries (reconnect_backoff).
  action streamOpenFail: bool = all {
    st.streamSt == DSOpening,
    st.openFailsLeft > 0,
    st' = { ...st, streamSt: DSDead, openFailsLeft: st.openFailsLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  /// A confirmed-open stream dies under the relay (server tear-down,
  /// failover fence, network): the StreamEnd::Closed|Error arm parks
  /// the relay; anything already in this stream's channel buffer (the
  /// in-flight cell) is unrecoverable. fault-stream budget.
  action streamDie: bool = all {
    st.streamSt == DSOpen,
    st.streamDeathsLeft > 0,
    val lostCell = st.inFlightStream == st.streamId
    all {
      st' = { ...st,
        streamSt: DSDead,
        relayTarget: 0,
        streamDeathsLeft: st.streamDeathsLeft - 1,
        inFlightStream: if (lostCell) 0 else st.inFlightStream },
      viol' = viol,
      reach' = reach
    }
  }

  /// The scheduler pushes the WorkAssignment over the open stream
  /// (environment): it carries the serving leader's current
  /// generation.
  action assignmentArrive: bool = all {
    not(st.exited),
    st.streamSt == DSOpen,
    st.asg == AsgNone,
    st' = { ...st, asg: AsgInFlight, asgGen: st.schedGen },
    viol' = viol,
    reach' = reach
  }

  /// runtime/mod.rs::handle_assignment — gate on the generation fence
  /// (is_stale_assignment), the SIGTERM drain flag, and the single
  /// BuildSlot (try_claim); no ACK either way. Accepting spawns the
  /// build task, which arms completion_pending BEFORE its first await
  /// (bug_012) — so acceptance and the arming are one step.
  action handleAssignment: bool = all {
    not(st.exited),
    st.asg == AsgInFlight,
    val stale = staleAssign(st.asgGen, st.watermark)
    val accept = and { not(stale), not(st.draining), st.slot == SlotIdle }
    all {
      st' = { ...st,
        asg: if (accept) AsgAccepted else AsgRejected,
        slot: if (accept) SlotClaimed else st.slot,
        completionPending: if (accept) true else st.completionPending },
      viol' = { ...viol,
        staleAssignAccepted: viol.staleAssignAccepted or (accept and stale) },
      reach' = reach
    }
  }

  /// The build reaches its terminal outcome (success, failure, cancel
  /// or panic — the outcome's content is irrelevant to delivery).
  action buildEnds: bool = all {
    not(st.exited),
    st.slot == SlotClaimed,
    not(st.buildDone),
    st' = { ...st, buildDone: true },
    viol' = viol,
    reach' = reach
  }

  /// runtime/result.rs::send_completion — the single chokepoint: every
  /// terminal outcome (the normal path and the panic-catcher's late
  /// completion) enqueues exactly one report into the permanent sink;
  /// completion_pending is (re)stored true before the enqueue.
  action enqueueSink: bool = all {
    not(st.exited),
    st.buildDone,
    not(st.sinkEverQueued),
    st' = { ...st, sink: true, sinkEverQueued: true, completionPending: true },
    viol' = viol,
    reach' = reach
  }

  /// The BuildSlotGuard drops (normal return after send_completion, or
  /// the unwind of a panicking build task BEFORE the catcher's late
  /// send — the bug_012 ordering): the slot frees independently of the
  /// enqueue.
  action slotRelease: bool = all {
    not(st.exited),
    st.buildDone,
    st.slot == SlotClaimed,
    st' = { ...st, slot: SlotDone },
    viol' = viol,
    reach' = reach
  }

  /// runtime/mod.rs::relay_loop — the relay picks the report from the
  /// sink (or its one-message buffer) and sends it into the CURRENT
  /// confirmed-open target's channel: the stream-local in-flight cell.
  /// completion_pending is cleared on this mpsc-send-Ok — "in the
  /// 256-cap buffer", NOT on the wire (the half-close drain covers the
  /// rest of the way on the exit path).
  action relayHandoff: bool = all {
    not(st.exited),
    st.relayTarget != 0,
    st.relayTarget == st.streamId,
    st.streamSt == DSOpen,
    st.inFlightStream == 0,
    st.sink or st.relayBuf,
    st' = { ...st,
      sink: false,
      relayBuf: false,
      inFlightStream: st.relayTarget,
      completionPending: false },
    viol' = { ...viol,
      relayedIntoUnconfirmed: viol.relayedIntoUnconfirmed or st.streamSt != DSOpen },
    reach' = reach
  }

  /// relay_loop's send error path: the target's channel is gone (the
  /// reconnect loop already parked or the stream died): the message is
  /// re-buffered in the relay's one-message cell and waits for the
  /// next swap; completion_pending stays set.
  action relayRebuffer: bool = all {
    not(st.exited),
    st.sink,
    or { st.relayTarget == 0, st.streamSt == DSDead },
    st' = { ...st, sink: false, relayBuf: true },
    viol' = viol,
    reach' = reach
  }

  /// The serving leader's endpoint receives the in-flight cell off a
  /// live stream (tonic flushes the channel; the worker-stream-reader
  /// forwards it). Only a live (or half-closing) stream delivers.
  action endpointReceive: bool = all {
    st.inFlightStream != 0,
    st.inFlightStream == st.streamId,
    st.streamSt == DSOpen,
    st' = { ...st, inFlightStream: 0, delivered: st.delivered + 1 },
    viol' = { ...viol, secondDelivery: viol.secondDelivery or st.delivered >= 1 },
    reach' = reach
  }

  /// SIGTERM lands (rio-common signal token): the next pass of the
  /// reconnect loop runs the drain transition
  /// (drain.rs::reconnect_drain_gate — set draining, spawn the
  /// build-flushed watcher); the stream stays connected so an owed
  /// completion can still flush.
  action sigtermArrives: bool = all {
    not(st.exited),
    not(st.sigterm),
    st' = { ...st, sigterm: true, draining: true },
    viol' = viol,
    reach' = reach
  }

  /// The serving generation moves (failover in the scheduler fleet):
  /// the builder observes it through a later heartbeat response and
  /// fetch_max-es its watermark (runtime/heartbeat.rs::apply — the B5
  /// latch). Observation is a separate step so an assignment from the
  /// old leader can still race ahead of the observation (the bounded
  /// dual-belief window the lease model prices).
  action genMove: bool = all {
    st.genMovesLeft > 0,
    st' = { ...st, schedGen: st.schedGen + 1, genMovesLeft: st.genMovesLeft - 1 },
    viol' = viol,
    reach' = reach
  }

  action observeGen: bool = all {
    not(st.exited),
    st.watermark < st.schedGen,
    st' = { ...st, watermark: st.schedGen },
    viol' = viol,
    reach' = reach
  }

  /// runtime/mod.rs::run, StreamEnd::BuildComplete arm + run_teardown —
  /// the graceful exit as one step: the build-done watcher
  /// (wait_build_flushed: slot idle AND completion_pending false), the
  /// drain-done notification, the reconnect-drain idle fast-path or
  /// the I-116 idle exit converge on the break; the relay is parked,
  /// tonic flushes the in-flight cell (END_STREAM + the bounded drain
  /// of the response side — the bug_117 half-close), and the process
  /// exits 0. Nothing else runs after the break in the real control
  /// flow, so the break, the flush and the exit are one action; the
  /// latch records an exit that left the report owed or unflushed
  /// (the bug_012/bug_117/bug_472 class).
  action gracefulExit: bool = all {
    not(st.exited),
    // the exit gates: a completed build whose report was handed off
    // (build_done watcher), a drain with nothing owed (idle fast-path /
    // drain watcher), or an idle timeout with no assignment ever
    // accepted
    or {
      and { st.buildDone, st.slot == SlotDone, not(st.completionPending) },
      and { st.draining, st.slot != SlotClaimed, not(st.completionPending) },
      and { st.asg != AsgAccepted, st.slot == SlotIdle, not(st.completionPending) }
    },
    val flushes = st.inFlightStream != 0 and st.inFlightStream == st.streamId and st.streamSt == DSOpen
    all {
      st' = { ...st,
        halfCloseRan: true,
        relayTarget: 0,
        inFlightStream: if (flushes) 0 else st.inFlightStream,
        delivered: if (flushes) st.delivered + 1 else st.delivered,
        streamSt: if (st.streamSt == DSOpen) DSDead else st.streamSt,
        exited: true,
        exitedGracefully: true },
      viol' = { ...viol,
        secondDelivery: viol.secondDelivery or (flushes and st.delivered >= 1),
        exitWithReportOwed: viol.exitWithReportOwed or
          st.completionPending or st.sink or st.relayBuf or
          (st.inFlightStream != 0 and not(flushes)) },
      reach' = { ...reach,
        halfCloseFlushed: reach.halfCloseFlushed or flushes }
    }
  }

  /// SIGKILL / OOM-kill / grace expiry (fault-process): the process is
  /// gone with no choreography; whatever was still local or in flight
  /// is lost. The scheduler-side repairs (Model S) own this outcome —
  /// the latches deliberately do NOT fire here (this is the "or death"
  /// arm of exactly-once-or-death).
  action sigkill: bool = all {
    not(st.exited),
    st.sigkillsLeft > 0,
    st' = { ...st,
      exited: true,
      exitedGracefully: false,
      sigkillsLeft: st.sigkillsLeft - 1,
      sink: false, relayBuf: false, inFlightStream: 0,
      streamSt: if (st.streamSt == DSOpen or st.streamSt == DSOpening) DSDead else st.streamSt,
      relayTarget: 0 },
    viol' = viol,
    reach' = reach
  }

  // --- 7. init ------------------------------------------------------------

  action init = all {
    st' = {
      streamSt: DSNone,
      streamId: 0,
      streamsOpened: 0,
      relayTarget: 0,
      sink: false,
      relayBuf: false,
      inFlightStream: 0,
      delivered: 0,
      slot: SlotIdle,
      asg: AsgNone,
      buildDone: false,
      completionPending: false,
      sinkEverQueued: false,
      sigterm: false,
      draining: false,
      halfCloseRan: false,
      exited: false,
      exitedGracefully: false,
      schedGen: 1,
      watermark: 1,
      asgGen: 1,
      openFailsLeft: OPEN_FAILS,
      streamDeathsLeft: STREAM_DEATHS,
      genMovesLeft: GEN_MOVES,
      sigkillsLeft: SIGKILLS
    },
    viol' = {
      relayedIntoUnconfirmed: false,
      exitWithReportOwed: false,
      staleAssignAccepted: false,
      reportLostWhileOwed: false,
      secondDelivery: false
    },
    reach' = {
      halfCloseFlushed: false
    }
  }

  // --- 8. step --------------------------------------------------------------

  action step = any {
    streamOpenStart,
    streamOpenOk,
    streamOpenFail,
    streamDie,
    assignmentArrive,
    handleAssignment,
    buildEnds,
    enqueueSink,
    slotRelease,
    relayHandoff,
    relayRebuffer,
    endpointReceive,
    sigtermArrives,
    genMove,
    observeGen,
    gracefulExit,
    sigkill
  }
}

// ===========================================================================
// Regimes
// ===========================================================================

/// Base regime: no stream faults, no kills, no failover. The happy
/// path — register, accept, build, enqueue, relay, deliver, half-close,
/// exit — plus the SIGTERM drain and idle-exit orderings.
module executorDeliveryBase {
  import executorDelivery(
    STREAM_CEIL = 2,
    STREAM_DEATHS = 0,
    OPEN_FAILS = 0,
    GEN_MOVES = 0,
    SIGKILLS = 0
  ).*
}

/// fault-stream: open attempts may fail to confirm, confirmed streams
/// may die under the relay (twice, so a death after the reconnect is
/// also explored), and the serving generation may move once — the
/// reconnect/park/re-buffer choreography and the watermark fence carry
/// the weight here.
module executorDeliveryFaultStream {
  import executorDelivery(
    STREAM_CEIL = 3,
    STREAM_DEATHS = 2,
    OPEN_FAILS = 1,
    GEN_MOVES = 1,
    SIGKILLS = 0
  ).*
}

/// fault-process: the pod can be killed without choreography (SIGKILL /
/// OOM / grace expiry) at any point, on top of one stream death — the
/// "or death" arm: no latch may fire, and the scheduler-side repair
/// obligation is Model S's.
module executorDeliveryFaultProcess {
  import executorDelivery(
    STREAM_CEIL = 3,
    STREAM_DEATHS = 1,
    OPEN_FAILS = 1,
    GEN_MOVES = 0,
    SIGKILLS = 1
  ).*
}
