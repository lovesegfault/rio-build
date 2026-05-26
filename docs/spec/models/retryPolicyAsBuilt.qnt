// -*- mode: Bluespec; -*-
//
// FROZEN (Phase 1c, retry-formal): this file is the Stage-B AS-BUILT
// encoding, frozen at the Phase-1b cutover under its pre-collapse
// semantics. It is imported only by the Stage-C calibration corpus
// (docs/spec/models/calibration/*.qnt, the `quint-retry-calib-*` checks),
// which replays the historical-fix reverts against the pre-collapse
// behaviour; it receives NO semantic edits during Phase 1 and is retired
// in Phase 2 together with that corpus when the acceptance table and MBT
// replace them (design §5, Phase-2 row). The post-collapse encoding —
// what production now does after the Phase-1b 9-site collapse — is the
// new `retryPolicy.qnt` main next to this file; every regime, witness and
// divergence check in nix/quint.nix points there.
//
// Model of the AS-BUILT (pre-Phase-1b) rio-scheduler retry/poison/cascade
// machinery: the ten `RetryState` counters, the nine cap-check entry
// points (E1-E9 in docs/spec/models/retry-invariant-map.md), the
// signal-channel dedup (`recently_disconnected` / `last_completed` /
// draining), the four best-effort PG mirror columns, the leader-failover
// selective forgiveness, and the poison/cascade/reset lifecycle.
//
// This is the retry-formal campaign's Stage-B deliverable (Phase 0 of the
// design in retry-formal-design.md §3). The transition relation encodes
// THE CODE AS IT WAS BEFORE THE PHASE-1B COLLAPSE, not the reference
// fold: every divergent arm the invariant map catalogs (D1: E7 poisons at
// the timeout cap where E4 cancels; D2: E6's at-cap increment does not
// stamp the window anchor; D3: E6's promoted arm charges nothing; the E2
// window-reset fall-through on exempt under-cap events; D4: E8's
// `failed_builders` charge never reaches PG) is reproduced as that code
// did it. The reference fold (rio-scheduler/src/retry_policy.rs) enters
// only as the specification ghost: a second copy of the counters advanced
// by the fold's own transition function at every observed accounting
// event, which `countersRefineHistory` and `verdictMatchesFold` compare
// the live state against. The pre-registered expected falsifications (the
// invariant map's Stage-B list) are exactly the divergences; their
// expect-violation checks were re-pointed at the post-collapse main and
// flipped to HOLD checks by Phase 1c.
//
// WHAT IS MODELED
//   - One building derivation (the full counter/budget lifecycle) plus
//     one dependent derivation (the cascade target; it is never
//     dispatched -- its only dependency never completes on the failure
//     paths this model is about).
//   - The physical attempt: dispatch to an executor slot, then exactly
//     one physical end per attempt -- a worker-reported failure class
//     (transient / infra-putpath / infra-cgroup-oom / infra-other /
//     permanent / timeout), a pod death (OOM-or-disk-pressure /
//     deadline-exceeded / non-resource crash), or a wedge (no report, no
//     death; the backstop's subject).
//   - The observation channels and their nondeterminism: the worker
//     CompletionReport (E1-E4), the stream-close / heartbeat-timeout
//     disconnect (E5), the controller termination report (E6/E7) which
//     may race ahead of the disconnect, arrive after it (through the
//     `recently_disconnected` correlation), arrive after the derivation
//     was already re-dispatched (the late installment), or never arrive;
//     the scheduler backstop timer (E8); the dispatch-time fleet-exhaust
//     check (E9).
//   - The dedup state that makes one death count at most once:
//     `recently_disconnected` (insert on mid-build disconnect only,
//     first-report-wins removal, TTL sweep), `last_completed` (the
//     post-completion disconnect and the race-ahead report both suppress
//     the insert), the one-shot draining flag, and the non-promoting
//     -reason early return (a no-op as built, so not an action).
//   - The ten counters as the code maintains them, the fold ghost, the
//     PG mirror row (retry_count / failed_builders / resubmit_cycles /
//     poisoned_at / status), and the best-effort nature of its writers
//     (a bounded number of mirror UPDATEs may be silently lost in the
//     fault regime; E8's failed_builders charge is NEVER mirrored --
//     structurally absent, not fault-injected).
//   - Leader failover: the executor map, dedup state, in-flight
//     observations and attempt are wiped; the derivation is rebuilt from
//     the PG row by the code's two constructors (poisoned rows via
//     `from_poisoned_row`, non-terminal rows via `from_recovery_row`,
//     TTL-expired poison cleared instead of reloaded), and the orphan
//     reconcile re-runs the threshold check over the recovered counters.
//   - The resets: the dag::merge resubmit reset (bounded for Poisoned by
//     POISON_RESUBMIT_RETRY_LIMIT, unconditional for Cancelled), the
//     cache-hit clear (clears 9 of 10 counters, keeps backoff_until),
//     and the admin-clear/TTL-expiry path (PG-first, node removed).
//   - The abstract clock: the 300 s infra window, the poison TTL, the
//     recently_disconnected TTL and the transient backoff are scaled to
//     small model constants with their production ordering preserved
//     (POISON_TTL > REPORT_TTL = WINDOW >= backoff base).
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - The resource-floor ladder (G6). The floor enters only as the
//     {promoted, at_cap} outcome each OOM-class event consumes, with a
//     single bounded promotion budget standing in for the ladder's
//     monotone doubling (so the promoted arm cannot recur unboundedly --
//     in production the ladder reaches the ceiling after finitely many
//     promotions). The cold-start {false,false} outcome for OOM-class
//     events is not generated. Per design §3 this prices G6 out of
//     Stage-C calibration (NOT-ENCODED); the ladder's own invariants
//     stay with floor.rs's unit tests.
//   - The substitution-failure path, the cancel paths (admin cancel,
//     per-build timeout, orphan-watcher), and build-level bookkeeping
//     (keep_going, summaries, events): out of the fold's scope per the
//     invariant map ("Out of scope" section); they consult no retry
//     counter and charge no budget.
//   - Executor identity is a reusable pod SLOT, not a fresh pod name per
//     attempt. A slot respawns (fresh registration, cleared
//     last_completed/draining) only once the controller has nothing
//     pending to report about its previous incarnation, which is what
//     keeps a late report from being mis-correlated to a later
//     incarnation -- the production guarantee that fresh pods have fresh
//     names. The placement consequence (failed_builders excludes the
//     slot forever) is faithful to hard_filter; the poison threshold is
//     scaled to the slot count so every threshold path stays reachable.
//   - Static eligibility (kind/system/features) is uniform across the
//     executor slots: eligibleFleet implements only the
//     registered/non-draining half of the design's eligibility/draining
//     predicate. A model-time narrowing of design §3's pre-registered G7
//     encoding -- the heterogeneous-fleet exhaust mis-fire class
//     (a62631c90) is not representable and is dispositioned NOT-ENC in
//     the calibration table; revisit if placement work adds
//     heterogeneous eligibility.
//   - Stream epochs (the I-056a stale-disconnect guard), heartbeat
//     bookkeeping, the SLA estimator, and the backstop's wall-clock
//     threshold (the backstop fires whenever an attempt is wedged; its
//     production threshold dwarfs every other timescale here).
//   - Worker reports racing ahead of their own stream close are not
//     reordered: a successfully-sent CompletionReport is processed
//     before the disconnect that follows it (gRPC stream ordering).
//   - The non-promoting controller termination report (Completed /
//     Error / EvictedOther): as built it returns before touching any
//     dedup or counter state, so it is a stuttering step; it is omitted
//     from the alphabet (its pre-fix behavior is a Stage-C revert).
//
// ENCODING DECISIONS
//   - The fold ghost (`spec`, `specVerdict`) is advanced by
//     `specApply` -- a transcription of retry_policy.rs::apply -- at
//     exactly the observed accounting events the dedup layer lets
//     through, mirroring the fold's definition of "the observed
//     history". The full event history is never materialized; the fold
//     is incremental, so carrying its state is equivalent to folding
//     the history at every state.
//   - At leader failover the fold ghost is re-seeded from the recovered
//     projection alongside the live counters: countersRefineHistory is
//     an intra-tenure invariant (the new leader's counters refine what
//     the new leader has observed since recovery); the cross-failover
//     loss is the subject of recoveryIsTheDocumentedProjection /
//     recoveryNeverFabricatesFailures / durableMirrorsCharges instead.
//   - Dispatch-time poisons (E9) and recovery-time poisons (the orphan
//     reconcile's threshold re-check) have no fold event -- the fold's
//     event alphabet is the failure-event alphabet. They are tracked via
//     poisonSrc and excluded from the counter/verdict refinement
//     comparisons; their own correctness is the placement invariant's
//     and the recovery invariants' subject.
//   - The poison threshold uses the production distinct-workers mode;
//     THRESHOLD is scaled to the executor-slot count so it is reachable.
//   - Verdict-channel-invariance is checked as verdict-level refinement
//     against the fold (the fold is channel-invariant by construction;
//     its unit tests pin that), scoped to the terminal budget verdicts.
//     The requeue-side divergence that D3 induces (the fold poisons at
//     the exempt cap the code never charges) is deliberately left to
//     countersRefineHistory, so the verdictMatchesFold falsification is
//     exactly the pre-registered D1 shape.
//
// The invariants, their witnesses and the expected-falsification wiring
// are documented at their definitions and in nix/quint.nix. Measured
// state counts, depths and wall-clocks live in the introducing commits'
// messages and the CI checks' transcripts, never in this file.
module retryPolicyAsBuilt {
  // --- 1. types ---------------------------------------------------------

  /// Abstract clock value (model "seconds" -- see the scaled constants).
  type TimeOpt = | NoTime | At(int)

  /// The ten RetryState counters. Used twice: `live` (as the code
  /// maintains them) and `spec` (as the reference fold computes them).
  type Counters = {
    count: int,
    resubmitCycles: int,
    infraCount: int,
    timeoutCount: int,
    lastInfraAt: TimeOpt,
    exemptInfraCount: int,
    failedBuilders: Set[str],
    failureCount: int,
    poisonedAt: TimeOpt,
    backoffUntil: TimeOpt,
  }

  /// Which budget's exhaustion produced a poison (the fold's
  /// PoisonReason; the live side keeps only a tag for witnesses).
  /// RTimeoutBudget is live-only: the as-built E7 poisons at the timeout
  /// cap (divergence D1); the fold never produces a timeout poison.
  type Reason =
    | RThreshold
    | RFleetExhausted
    | RTransientBudget
    | RInfraBudget
    | RExemptInfraBudget
    | RPermanent
    | RTimeoutBudget

  /// The fold's budget verdict (TtlExpire is computed from `now` at the
  /// comparison point, not stored).
  type Verdict = | VRequeue | VPoison(Reason) | VCancel

  /// Building-derivation status, collapsed to what the budget machinery
  /// distinguishes: Created/Queued/Ready collapse to DReady,
  /// Assigned/Running to DRunning, the Failed intermediate is invisible
  /// (reset_to_ready passes through it atomically). DAbsent = removed
  /// from the DAG (admin clear / TTL expiry); DCompleted = cache hit.
  type DStatus = | DReady | DRunning | DPoisoned | DCancelled | DCompleted | DAbsent

  /// Dependent-derivation status (the cascade target).
  type DepStatus = | DepReady | DepFailed

  /// What poisoned the building derivation (scoping for the fold
  /// comparisons -- see the header's encoding decisions).
  type PoisonSrc = | NoPoison | EventPoison | DispatchPoison | RecoveryPoison

  /// The PG mirror of the derivations row's retry-relevant columns, as
  /// the four best-effort writers leave it.
  type PgStatus = | PgNonTerminal | PgPoisoned | PgCancelled | PgCompleted
  type PgRow = {
    status: PgStatus,
    retryCount: int,
    failedBuilders: Set[str],
    resubmitCycles: int,
    poisonedAt: TimeOpt,
  }

  /// Worker CompletionReport classes (the E1-E4 triggers).
  type ReportClass =
    | RcTransient
    | RcInfraPutPath
    | RcInfraOom
    | RcInfraOther
    | RcPermanent
    | RcTimeout

  /// Pod-death kinds (the E5/E6/E7 triggers). The deadline overrun whose
  /// worker report ALSO got out before the kill (the dual-observation
  /// shape) is the `dual` flag on a Reported{RcTimeout} attempt instead.
  type DeathKind = | DkOom | DkDeadline | DkCrash

  /// The in-flight physical attempt and its not-yet-processed end.
  type Attempt =
    | NoAttempt
    | Running(str)
    | Reported({ exec: str, class: ReportClass, dual: bool })
    | PodDead({ exec: str, kind: DeathKind })
    | Wedged(str)

  /// Pending controller termination report for an executor slot's most
  /// recent death (the controller re-reports until JOB_TTL; one
  /// effective delivery is all the as-built code can consume).
  type CtrlOpt = | NoCtrl | CtrlOom | CtrlDeadline

  /// One executor slot, as the entry points read it. `ctrlStale` marks a
  /// pending controller report that refers to a PREVIOUS incarnation of
  /// the slot (the pod it names no longer exists): it can still be
  /// correlated through a recently_disconnected entry, but the race-ahead
  /// live-executor lookup must not match it -- in production the old pod
  /// name simply misses the executor map.
  type ExecRec = {
    registered: bool,
    draining: bool,
    runningOurs: bool,
    lastCompletedOurs: bool,
    rd: TimeOpt,        // recently_disconnected entry (insertion time)
    ctrl: CtrlOpt,      // pending controller report for the last death
    ctrlStale: bool,    // ...left over from a previous incarnation
    rdForCtrl: bool,    // the rd entry is the pending report's own death's
    deathCharges: int,  // GHOST: counted accounting events for that death
  }

  /// The fold's event alphabet (retry_policy.rs::AttemptEvent), minus
  /// the executor field where the fold ignores it.
  type FoldEvent =
    | FTransient({ at: int, exec: str })
    | FInfra({ at: int, exempt: bool, atCap: bool })
    | FPermanent({ at: int, exec: str })
    | FWorkerTimeout(int)
    | FDisconnect(int)
    | FControllerTermination({ at: int, promoted: bool, atCap: bool })
    | FControllerDeadlineExceeded(int)
    | FBackstopTimeout({ at: int, exec: str })
    | FDispatched(int)
    | FResubmitReset(int)
    | FCacheHitClear(int)
    | FPoisonCleared(int)

  /// Which entry point (or reset path) the most recent accounting
  /// observation went through, plus per-observation facts the
  /// per-event invariants and witnesses read. Overwritten by every
  /// accounting action; ONone between them.
  type ObsKind =
    | ONone | OE1 | OE2 | OE3 | OE4 | OE5 | OE6 | OE7 | OE8
    | OE9Dispatch | OResubmit | OCacheHit | OPoisonClear | OTtlExpire | OFailover
  type LastObs = {
    kind: ObsKind,
    chargedAny: bool,       // some live counter was charged by this event
    doubleCharge: bool,     // the same budget was charged twice by this event
    windowReset: bool,      // the I-127 stale-window reset fired
    exemptFallthrough: bool,// ...and it fired on an exempt under-cap event
    promoted: bool,         // the event's floor outcome
    atCap: bool,
    lateInstallment: bool,  // controller report correlated through an rd entry
    raceAhead: bool,        // controller report used the still-live executor
    terminal: bool,         // the event produced a terminal state
  }

  // --- 2. constants (const = overridable per regime) --------------------

  /// Executor slots. Two are required for the distinct-worker poison
  /// threshold, the placement exclusion and the fleet-exhaust check; the
  /// channel regimes run with one (the channel fan-out is per-death and
  /// does not need a second slot).
  const EXECUTORS: Set[str]
  /// Clock ceiling (action precondition on tick).
  const MAX_TIME: int
  /// Ceiling on physical attempts (action precondition on dispatch).
  const MAX_ATTEMPTS: int
  /// The bound the boundedness clause of sched.retry.attempts-bounded+2
  /// justifies for the regime's enabled alphabet (only consulted by
  /// attemptsBoundedGlobal).
  const ATTEMPT_BOUND: int
  /// Leader failovers allowed (0 outside the failover regime).
  const MAX_FAILOVERS: int
  /// Best-effort PG mirror writes that may be silently lost (0 outside
  /// the fault-persist regime).
  const PG_FAULTS: int
  /// Channel/alphabet switches (one per regime concern).
  const ENABLE_WORKER: bool     // worker-reported failure classes (E1-E4)
  const ENABLE_PODDEATH: bool   // pod deaths + controller channel (E5-E7)
  const ENABLE_CRASH: bool      // the no-report crash death kind (C2)
  const ENABLE_WEDGE: bool      // the wedge end + backstop (E8)
  const ENABLE_RESETS: bool     // resubmit / cache-hit / TTL-admin clear

  // Scaled budget (the production values are configuration, not
  // normative numbers -- sched.retry.attempts-bounded+2). The scaling
  // keeps every cap reachable in a small state space; the witnesses
  // prove each scaled cap is still reached.
  pure val MAX_RETRIES = 2          // RetryPolicy.max_retries
  pure val MAX_INFRA = 2            // .max_infra_retries
  pure val MAX_TIMEOUT = 2          // .max_timeout_retries
  pure val MAX_EXEMPT = 2           // .max_exempt_infra_retries
  pure val INFRA_WINDOW = 1         // .infra_retry_window_secs
  pure val BACKOFF_BASE = 1         // .backoff_base_secs
  pure val BACKOFF_MULT = 2         // .backoff_multiplier
  pure val BACKOFF_CAP = 2          // .backoff_max_secs
  pure val THRESHOLD = 2            // PoisonConfig.threshold (distinct mode)
  pure val RESUBMIT_LIMIT = 1       // POISON_RESUBMIT_RETRY_LIMIT
  pure val POISON_TTL = 2           // POISON_TTL
  pure val REPORT_TTL = 1           // TERMINATION_REPORT_TTL
  /// The floor-ladder length abstraction: how many promoted outcomes the
  /// ladder can yield before only at_cap remains.
  pure val MAX_PROMOTIONS = 1

  // --- 3. pure defs -----------------------------------------------------

  pure val zeroCounters: Counters = {
    count: 0, resubmitCycles: 0, infraCount: 0, timeoutCount: 0,
    lastInfraAt: NoTime, exemptInfraCount: 0, failedBuilders: Set(),
    failureCount: 0, poisonedAt: NoTime, backoffUntil: NoTime,
  }

  pure val freshPg: PgRow = {
    status: PgNonTerminal, retryCount: 0, failedBuilders: Set(),
    resubmitCycles: 0, poisonedAt: NoTime,
  }

  pure val freshExec: ExecRec = {
    registered: true, draining: false, runningOurs: false,
    lastCompletedOurs: false, rd: NoTime, ctrl: NoCtrl, ctrlStale: false,
    rdForCtrl: false, deathCharges: 0,
  }

  pure val noObs: LastObs = {
    kind: ONone, chargedAny: false, doubleCharge: false, windowReset: false,
    exemptFallthrough: false, promoted: false, atCap: false,
    lateInstallment: false, raceAhead: false, terminal: false,
  }

  /// RetryPolicy::backoff_duration without the jitter: min(base*mult^n, cap).
  pure def backoffSecs(attempt: int): int =
    val unclamped = if (attempt <= 0) BACKOFF_BASE
      else if (attempt == 1) BACKOFF_BASE * BACKOFF_MULT
      else BACKOFF_BASE * BACKOFF_MULT * BACKOFF_MULT
    if (unclamped >= BACKOFF_CAP) BACKOFF_CAP else unclamped

  /// PoisonConfig::is_poisoned, distinct-workers mode (production default).
  pure def thresholdReached(c: Counters): bool =
    c.failedBuilders.size() >= THRESHOLD

  /// failed_builders_exhausts_fleet (dispatch.rs): every statically
  /// eligible, registered, non-draining worker is in the exclusion set;
  /// an empty exclusion set or an empty eligible fleet never exhausts
  /// (sched.dispatch.fleet-exhaust+3's empty-fleet defer).
  pure def exhaustsFleet(failed: Set[str], eligible: Set[str]): bool = and {
    failed != Set(),
    eligible != Set(),
    eligible.forall(w => failed.contains(w)),
  }

  /// The documented post-failover projection of the persisted columns
  /// for a NON-TERMINAL row (state/derivation.rs::from_recovery_row):
  /// count and resubmit_cycles and failed_builders recovered,
  /// failure_count derived as failed_builders.len(), the rest defaulted.
  pure def recoveryProjectionNonTerminal(pgRow: PgRow): Counters = {
    ...zeroCounters,
    count: pgRow.retryCount,
    resubmitCycles: pgRow.resubmitCycles,
    failedBuilders: pgRow.failedBuilders,
    failureCount: pgRow.failedBuilders.size(),
  }

  /// The documented post-failover projection for a POISONED row
  /// (state/derivation.rs::from_poisoned_row): resubmit_cycles,
  /// failed_builders, poisoned_at recovered, failure_count derived; note
  /// the row's retry_count is NOT read by this constructor (the
  /// recovery-projection rule and retry_policy.rs::recovery_projection
  /// describe the non-terminal constructor; the map's Stage-B section
  /// records the discrepancy).
  pure def recoveryProjectionPoisoned(pgRow: PgRow): Counters = {
    ...zeroCounters,
    resubmitCycles: pgRow.resubmitCycles,
    failedBuilders: pgRow.failedBuilders,
    failureCount: pgRow.failedBuilders.size(),
    poisonedAt: pgRow.poisonedAt,
  }

  /// RetryState::clear() -- nine of the ten fields; backoff_until survives.
  pure def clearForCacheHit(c: Counters): Counters =
    { ...zeroCounters, backoffUntil: c.backoffUntil }

  // -- The reference fold (retry_policy.rs::apply), transcribed ---------
  //
  // This is the specification oracle, NOT the code: its D1/D2/D3
  // adjudications deliberately deviate from the entry points (the fold
  // stamps the window anchor on every infra_count increment, charges
  // exempt_infra_count for promoted controller terminations, and
  // produces Cancel for the controller-observed timeout-cap exhaustion).
  // Keep in lockstep with rio-scheduler/src/retry_policy.rs.

  pure def specApply(c0: Counters, ev: FoldEvent, eligible: Set[str]): { c: Counters, v: Verdict } =
    match ev {
      // E1: handle_transient_failure -- record, threshold, fleet, count cap.
      | FTransient(e) =>
        val c1 = { ...c0,
          failedBuilders: c0.failedBuilders.union(Set(e.exec)),
          failureCount: c0.failureCount + 1 }
        if (thresholdReached(c1))
          { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RThreshold) }
        else if (exhaustsFleet(c1.failedBuilders, eligible))
          { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RFleetExhausted) }
        else if (c1.count < MAX_RETRIES)
          { c: { ...c1,
              count: c1.count + 1,
              backoffUntil: At(e.at + backoffSecs(c1.count)) },
            v: VRequeue }
        else
          { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RTransientBudget) }

      // E2: handle_infrastructure_failure -- exempt block (increment,
      // cap, NO early return under cap), I-127 window reset, non-exempt
      // cap check then charge + anchor stamp.
      | FInfra(e) =>
        val cExempt = if (e.exempt) { ...c0, exemptInfraCount: c0.exemptInfraCount + 1 } else c0
        if (e.exempt and cExempt.exemptInfraCount >= MAX_EXEMPT)
          { c: { ...cExempt, poisonedAt: At(e.at) }, v: VPoison(RExemptInfraBudget) }
        else
          val cReset =
            if (not(e.atCap) and
                match cExempt.lastInfraAt {
                  | NoTime => false
                  | At(last) => e.at - last > INFRA_WINDOW
                })
              { ...cExempt, infraCount: 0 }
            else cExempt
          if (e.exempt)
            { c: cReset, v: VRequeue }
          else if (cReset.infraCount >= MAX_INFRA)
            { c: { ...cReset, poisonedAt: At(e.at) }, v: VPoison(RInfraBudget) }
          else
            { c: { ...cReset, infraCount: cReset.infraCount + 1, lastInfraAt: At(e.at) },
              v: VRequeue }

      // E3: handle_permanent_failure -- poison + diagnostics-only insert.
      | FPermanent(e) =>
        { c: { ...c0,
            poisonedAt: At(e.at),
            failedBuilders: c0.failedBuilders.union(Set(e.exec)) },
          v: VPoison(RPermanent) }

      // E4: handle_timeout_failure -- check-then-increment; Cancel at cap.
      | FWorkerTimeout(at) =>
        if (c0.timeoutCount < MAX_TIMEOUT)
          { c: { ...c0, timeoutCount: c0.timeoutCount + 1 }, v: VRequeue }
        else
          { c: c0, v: VCancel }

      // E5: reassign_derivations -- charges nothing, re-checks the threshold.
      | FDisconnect(at) =>
        if (thresholdReached(c0))
          { c: { ...c0, poisonedAt: At(at) }, v: VPoison(RThreshold) }
        else
          { c: c0, v: VRequeue }

      // E6: handle_executor_termination. DIVERGENCE D2: the fold stamps
      // the window anchor on the at-cap increment; DIVERGENCE D3: the
      // fold charges exempt_infra_count on the promoted arm.
      | FControllerTermination(e) =>
        if (e.atCap)
          if (c0.infraCount >= MAX_INFRA)
            { c: { ...c0, poisonedAt: At(e.at) }, v: VPoison(RInfraBudget) }
          else
            { c: { ...c0, infraCount: c0.infraCount + 1, lastInfraAt: At(e.at) }, v: VRequeue }
        else if (e.promoted)
          val c1 = { ...c0, exemptInfraCount: c0.exemptInfraCount + 1 }
          if (c1.exemptInfraCount >= MAX_EXEMPT)
            { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RExemptInfraBudget) }
          else
            { c: c1, v: VRequeue }
        else
          { c: c0, v: VRequeue }

      // E7: handle_deadline_exceeded. DIVERGENCE D1: the fold produces
      // the spec-mandated Cancel at the cap; the as-built E7 poisons.
      | FControllerDeadlineExceeded(at) =>
        if (c0.timeoutCount >= MAX_TIMEOUT)
          { c: c0, v: VCancel }
        else
          { c: { ...c0, timeoutCount: c0.timeoutCount + 1 }, v: VRequeue }

      // E8: tick_process_backstop_timeouts -- record + threshold re-check.
      | FBackstopTimeout(e) =>
        val c1 = { ...c0,
          failedBuilders: c0.failedBuilders.union(Set(e.exec)),
          failureCount: c0.failureCount + 1 }
        if (thresholdReached(c1))
          { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RThreshold) }
        else
          { c: c1, v: VRequeue }

      // assign_to_worker -- clears the backoff defer.
      | FDispatched(at) =>
        { c: { ...c0, backoffUntil: NoTime }, v: VRequeue }

      // dag::merge resubmit reset -- fresh per-cycle state, cycle counted.
      | FResubmitReset(at) =>
        { c: { ...zeroCounters, resubmitCycles: c0.resubmitCycles + 1 }, v: VRequeue }

      // cache-hit clear -- nine of ten fields; backoff survives.
      | FCacheHitClear(at) =>
        { c: clearForCacheHit(c0), v: VRequeue }

      // admin ClearPoison / TTL expiry -- full reset.
      | FPoisonCleared(at) =>
        { c: zeroCounters, v: VRequeue }
    }

  // --- 4. state variables -----------------------------------------------

  /// The abstract clock.
  var now: int

  /// Building derivation: scheduler-visible status, the live counters
  /// (RetryState as the code maintains it), how it was poisoned (if it
  /// was), and the executor of the in-flight attempt.
  var dStatus: DStatus
  var live: Counters
  var poisonSrc: PoisonSrc

  /// The reference-fold ghost: the counters and verdict the fold
  /// computes over the observed accounting-event history of the current
  /// leader tenure.
  var spec: Counters
  var specVerdict: Verdict

  /// The PG mirror row for the building derivation.
  var pg: PgRow

  /// The dependent derivation (cascade target).
  var depStatus: DepStatus
  /// GHOST: the building derivation has at some point reached a
  /// failure-terminal state (justifies the dependent's DepFailed).
  var everFailureTerminal: bool

  /// The in-flight physical attempt and its unprocessed end.
  var attempt: Attempt
  /// Executor slots.
  var execs: str -> ExecRec
  /// Floor-ladder abstraction: promoted outcomes still available.
  var promotionsLeft: int

  /// GHOST: physical attempts started (dispatches).
  var physAttempts: int
  /// GHOST: every executor that ever had a charging failure recorded
  /// against it (any poison cycle) -- the truth the durable mirror is
  /// bounded by.
  var everFailedBuilders: Set[str]
  /// Failovers / PG-fault budgets remaining.
  var failoversLeft: int
  var pgFaultsLeft: int
  /// GHOST: the most recent accounting observation (per-event invariants
  /// and witnesses read this; overwritten each time).
  var last: LastObs
  /// GHOST: poison-reason tag of the most recent poison (witnesses).
  var lastPoisonReason: Reason
  /// GHOST: true exactly in the state produced by the failover action
  /// (cleared by every subsequent action); scopes the recovery-projection
  /// invariant to the post-failover state.
  var justFailedOver: bool

  // --- 5. derived state ---------------------------------------------------

  /// The eligible fleet as the fleet-exhaust check sees it: registered,
  /// non-draining slots (static eligibility -- kind/system/features --
  /// is uniform here).
  def eligibleFleet: Set[str] =
    EXECUTORS.filter(w => execs.get(w).registered and not(execs.get(w).draining))

  /// Placement candidates for dispatch: eligible and not excluded by
  /// hard_filter's failed_builders check, and idle.
  def placeable: Set[str] =
    eligibleFleet.filter(w => and {
      not(live.failedBuilders.contains(w)),
      not(execs.get(w).runningOurs),
    })

  /// The building derivation is failure-terminal (cascade trigger).
  def failureTerminal: bool = dStatus == DPoisoned or dStatus == DCancelled

  /// The backoff defer is over (dispatch_ready's check).
  def backoffElapsed: bool =
    match live.backoffUntil { | NoTime => true | At(t) => now >= t }

  /// The poison TTL has elapsed for the live poison (housekeeping scan).
  def poisonTtlElapsed: bool =
    match live.poisonedAt { | NoTime => false | At(t) => now - t > POISON_TTL }

  // --- 6. invariants ------------------------------------------------------
  // (defined before the actions; Quint definitions are order-independent)

  /// Structural sanity: ceilings hold, statuses and slots are mutually
  /// consistent, at most one attempt is in flight and it is on a slot
  /// that thinks it is running ours.
  val boundsOK = and {
    now >= 0 and now <= MAX_TIME,
    physAttempts >= 0 and physAttempts <= MAX_ATTEMPTS,
    promotionsLeft >= 0 and promotionsLeft <= MAX_PROMOTIONS,
    failoversLeft >= 0 and failoversLeft <= MAX_FAILOVERS,
    pgFaultsLeft >= 0 and pgFaultsLeft <= PG_FAULTS,
    live.count >= 0 and live.count <= MAX_RETRIES,
    live.infraCount >= 0 and live.infraCount <= MAX_INFRA,
    live.timeoutCount >= 0 and live.timeoutCount <= MAX_TIMEOUT,
    live.exemptInfraCount >= 0 and live.exemptInfraCount <= MAX_EXEMPT,
    live.failedBuilders.subseteq(EXECUTORS),
    pg.failedBuilders.subseteq(EXECUTORS),
    // the in-flight attempt is consistent with the scheduler view (the
    // converse does not hold: a late controller report can poison the
    // derivation while its next attempt is still in flight -- the
    // zombie execution whose eventual completion the status guard drops)
    (dStatus == DRunning) implies (attempt != NoAttempt),
    match attempt {
      | NoAttempt => EXECUTORS.forall(w => not(execs.get(w).runningOurs))
      | Running(w) => execs.get(w).runningOurs
      | Reported(r) => execs.get(r.exec).runningOurs
      | PodDead(p) => execs.get(p.exec).runningOurs
      | Wedged(w) => execs.get(w).runningOurs
    },
    // poisoned iff poisoned_at is stamped
    (dStatus == DPoisoned) implies live.poisonedAt != NoTime,
    (poisonSrc != NoPoison) iff (dStatus == DPoisoned),
  }

  /// The model-checked form of sched.retry.counters-refine-history+2:
  /// the live counters equal the reference fold of the observed history
  /// (carried incrementally as `spec`). poisoned_at is excluded when the
  /// poison came from a site outside the fold's event alphabet (the
  /// dispatch-time fleet exhaust, the recovery reconcile).
  /// EXPECTED FALSIFICATION (pre-registered): histories reaching D2
  /// (at-cap controller termination: the missing anchor stamp) or D3
  /// (promoted controller termination: the missing exempt charge).
  val countersRefineHistory =
    if (poisonSrc == DispatchPoison or poisonSrc == RecoveryPoison)
      { ...live, poisonedAt: NoTime } == { ...spec, poisonedAt: NoTime }
    else
      live == spec

  /// The budget verdict is channel-invariant: the live terminal state
  /// agrees with the fold's verdict over the observed history (the fold
  /// is channel-invariant by construction, so live-vs-fold agreement is
  /// the single-trace form of sched.retry.verdict-channel-invariant,
  /// scoped to the terminal budget verdicts).
  /// EXPECTED FALSIFICATION (pre-registered): the D1 history -- the
  /// timeout budget exhausted via E7 lands as Poisoned where the fold
  /// (and E4) produce Cancelled.
  val verdictMatchesFold = and {
    (dStatus == DPoisoned and poisonSrc == EventPoison) implies
      match specVerdict { | VPoison(r) => true | VRequeue => false | VCancel => false },
    dStatus == DCancelled implies specVerdict == VCancel,
  }

  /// sched.retry.attempts-bounded+2, the per-event clauses: a single
  /// accounting observation never charges the same budget twice, and
  /// every worker-reported failure or backstop observation either
  /// charges at least one budget or produces a terminal state.
  val attemptsChargedOnce = and {
    not(last.doubleCharge),
    (last.kind == OE1 or last.kind == OE2 or last.kind == OE3 or
     last.kind == OE4 or last.kind == OE8)
      implies (last.chargedAny or last.terminal),
  }

  /// sched.retry.attempts-bounded+2, the boundedness clause: the number
  /// of physical attempts is bounded by what the enabled budgets
  /// justify (ATTEMPT_BOUND is a regime constant).
  /// EXPECTED FALSIFICATION (pre-registered): the C2 no-report
  /// hard-crash loop -- an uncounted disconnect-requeue cycle bounded by
  /// nothing in the retry machinery.
  val attemptsBoundedGlobal = physAttempts <= ATTEMPT_BOUND

  /// The model-checked form of sched.retry.no-double-count: one
  /// physical executor death produces at most one counted accounting
  /// event for the derivation, over every observation subset and order
  /// the channels can produce.
  val noDoubleCount = EXECUTORS.forall(w => execs.get(w).deathCharges <= 1)

  /// Poison is terminal until cleared: while poisoned, no accounting
  /// event charges the counters (the entry points' status guards hold --
  /// the only charging observation that may coincide with DPoisoned is
  /// the one that produced the poison itself, marked terminal), the
  /// poison stamp stays present, and the only exits are the sanctioned
  /// ones (TTL expiry, cache hit, bounded resubmit, failover recovery) --
  /// the latter is structural: no other action's effect leaves DPoisoned.
  val poisonIsTerminalUntilCleared = and {
    (dStatus == DPoisoned and last.chargedAny) implies last.terminal,
    dStatus == DPoisoned implies live.poisonedAt != NoTime,
  }

  /// The cascade reaches exactly the dependents: a DepFailed dependent is
  /// justified by the building derivation having been failure-terminal,
  /// and whenever the building derivation is failure-terminal the
  /// not-yet-started dependent has been cascaded (the cascade is atomic
  /// with the terminal transition; the resubmit re-merge re-seeds it).
  val cascadeReachesExactlyTheDependents = and {
    depStatus == DepFailed implies everFailureTerminal,
    failureTerminal implies depStatus == DepFailed,
  }

  /// Post-failover state equals the documented projection of the
  /// persisted row (4 recovered / 1 derived / 5 defaulted; poisoned rows
  /// via from_poisoned_row, non-terminal rows via from_recovery_row).
  val recoveryIsTheDocumentedProjection =
    justFailedOver implies
      if (dStatus == DPoisoned and poisonSrc != RecoveryPoison)
        live == recoveryProjectionPoisoned(pg)
      else if (dStatus == DReady)
        live == recoveryProjectionNonTerminal(pg)
      else true

  /// No recovered counter exceeds what the durable rows support, and the
  /// durable rows themselves never claim more than the history actually
  /// produced (the mirror writers only lose writes, never invent them):
  /// every executor PG names as a failed builder really did have a
  /// charging failure recorded against it at some point, and the
  /// recovered exclusion set comes from the row alone.
  val recoveryNeverFabricatesFailures = and {
    pg.failedBuilders.subseteq(everFailedBuilders),
    justFailedOver implies live.failedBuilders.subseteq(pg.failedBuilders),
  }

  /// The durable view contains every charge the mirror protocol claims
  /// to mirror: an executor recorded into the live failed_builders set is
  /// also in the PG column.
  /// EXPECTED FALSIFICATION (pre-registered, the durable face of D4):
  /// E8's backstop charge has no append_failed_worker call, so the PG
  /// column permanently misses it (and, in the fault regime, a lost
  /// append_failed_worker does the same).
  val durableMirrorsCharges = live.failedBuilders.subseteq(pg.failedBuilders)

  /// The fleet-exhaust poison is sound: it only fires when the exclusion
  /// set covers a non-empty eligible fleet (the empty fleet defers --
  /// sched.dispatch.fleet-exhaust+3).
  val placementSound =
    (dStatus == DPoisoned and poisonSrc == DispatchPoison) implies
      lastPoisonReason == RFleetExhausted

  /// A poison clear that took effect in memory has taken effect durably:
  /// the admin/TTL clear is PG-first (a failed PG clear aborts the whole
  /// clear -- sched.admin.clear-poison's both-stores obligation), so in
  /// the state right after a clear observation the durable row is never
  /// still poisoned. Deliberately scoped to the TTL/admin path: the
  /// cache-hit clear is best-effort by design (a lost clear_poison there
  /// is reconciled by the next recovery, not by retry), so OCacheHit is
  /// not in the antecedent. Stage-C calibration invariant: the
  /// pre-b874e5120 in-mem-first ordering falsifies it.
  val clearedPoisonClearsDurably =
    (last.kind == OTtlExpire or last.kind == OPoisonClear) implies
      pg.status != PgPoisoned

  /// The TTL/admin clear scrubs the durable exclusion set too:
  /// db::clear_poison resets failed_builders (and the per-cycle retry
  /// count) in the same UPDATE that clears the status, so in the state
  /// right after a clear observation the durable row carries no
  /// failed-builder exclusions for the next cycle to inherit at
  /// recovery. Same antecedent scoping as clearedPoisonClearsDurably
  /// (the cache-hit clear is best-effort by design, so OCacheHit is not
  /// in the antecedent). Stage-C calibration invariant: the
  /// pre-b09c5b312 (X13) TTL clear -- which left PG failed_workers
  /// behind -- falsifies it.
  val clearedPoisonScrubsExclusions =
    (last.kind == OTtlExpire or last.kind == OPoisonClear) implies
      pg.failedBuilders == Set()

  /// Failover preserves the poison set: a durable row that still says
  /// Poisoned (and is not TTL-expired) is reloaded as Poisoned -- the
  /// "poison set preserved exactly minus TTL-expired entries" clause of
  /// sched.retry.recovery-projection that the counter-projection
  /// equality alone cannot see (it is conditioned on the recovered
  /// status). DCompleted is excluded from the antecedent: a cache-hit
  /// clear whose best-effort PG clear was lost legitimately leaves a
  /// stale Poisoned row behind a Completed node, and the gone-arm
  /// preserves the completion. Stage-C calibration invariant: a
  /// recovery that drops poisoned rows (the 891a6520d shape) falsifies
  /// it.
  val recoveryPreservesPoisonStatus =
    justFailedOver implies (
      and {
        pg.status == PgPoisoned,
        match pg.poisonedAt { | NoTime => true | At(t) => now - t <= POISON_TTL },
        dStatus != DCompleted,
      } implies dStatus == DPoisoned
    )

  // --- 6b. expect-violation witnesses (non-vacuity probes) ---------------
  // Each states "the interesting thing never happens"; the wired check
  // passes only when the checker violates it. None of these are
  // invariants.

  /// Violated when the transient budget's cap poisons.
  val noTransientCapPoison = not(dStatus == DPoisoned and lastPoisonReason == RTransientBudget)
  /// Violated when the non-exempt infra budget's cap poisons.
  val noInfraCapPoison = not(dStatus == DPoisoned and lastPoisonReason == RInfraBudget)
  /// Violated when the exempt-infra budget's cap poisons.
  val noExemptCapPoison = not(dStatus == DPoisoned and lastPoisonReason == RExemptInfraBudget)
  /// Violated when the distinct-worker poison threshold fires.
  val noThresholdPoison = not(dStatus == DPoisoned and lastPoisonReason == RThreshold)
  /// Violated when the fleet-exhaust backstop poisons.
  val noFleetExhaustPoison = not(dStatus == DPoisoned and lastPoisonReason == RFleetExhausted)
  /// Violated when the worker-reported timeout cap produces terminal Cancelled.
  val noTimeoutCapCancel = not(dStatus == DCancelled)
  /// Violated when the controller-reported timeout cap poisons (the D1 arm).
  val noControllerCapPoison = not(dStatus == DPoisoned and lastPoisonReason == RTimeoutBudget)
  /// Violated when the I-127 stale-window reset fires.
  val noWindowReset = not(last.windowReset)
  /// Violated when the window reset fires on an exempt under-cap event
  /// (the as-built E2 fall-through the corrected fold reproduces).
  val noExemptFallthroughReset = not(last.exemptFallthrough)
  /// Violated when a promoted controller termination is processed (D3 shape).
  val noPromotedTermination = not(last.kind == OE6 and last.promoted)
  /// Violated when an at-cap controller termination charges (D2 shape).
  val noAtCapTermination = not(last.kind == OE6 and last.atCap and last.chargedAny)
  /// Violated when a controller report is correlated through a
  /// recently_disconnected entry (the late installment).
  val noLateInstallment = not(last.lateInstallment)
  /// Violated when a controller report races ahead of the disconnect.
  val noRaceAheadReport = not(last.raceAhead)
  /// Violated when the backstop fires.
  val noBackstop = not(last.kind == OE8)
  /// Violated when a poison TTL expiry clears the derivation.
  val noTtlExpiry = not(last.kind == OTtlExpire)
  /// Violated when a resubmit reset starts a second poison cycle.
  val noResubmitReset = not(live.resubmitCycles > 0)
  /// Violated when a cache hit clears a poisoned derivation.
  val noCacheHitClear = not(last.kind == OCacheHit)
  /// Violated when the dependent is cascaded.
  val noCascade = not(depStatus == DepFailed)
  /// Violated when a leader failover happens with a non-empty,
  /// under-budget recovered history.
  val noFailoverWithHistory = not(and {
    justFailedOver,
    pg.failedBuilders != Set() or pg.retryCount > 0,
    dStatus == DReady,
  })
  /// Violated when a best-effort PG mirror write is lost.
  val noPgWriteLost = not(pgFaultsLeft < PG_FAULTS)
  /// Violated when a controller report charges (or poisons) for a death
  /// whose disconnect was already processed -- both channels of one
  /// death observed, the second one counted (the dedup's contended shape).
  val noDualObservation = not(last.lateInstallment and (last.chargedAny or last.terminal))

  // --- 7. actions ---------------------------------------------------------
  // Every action's precondition is the corresponding code path's guard
  // and its effect is that path's state mutation; the entry point it
  // mirrors is named in its doc comment. Actions are guarded (disabled
  // when their preconditions fail), not always-enabled no-ops.

  /// Advance the abstract clock and run the recently_disconnected TTL
  /// sweep (executor.rs::tick_sweep_recently_disconnected runs on every
  /// scheduler tick, so the sweep is folded into time advancement).
  action tick = all {
    now < MAX_TIME,
    now' = now + 1,
    execs' = EXECUTORS.mapBy(w =>
      val e = execs.get(w)
      match e.rd {
        | NoTime => e
        | At(t) => if (now + 1 - t > REPORT_TTL) { ...e, rd: NoTime } else e
      }),
    dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7a. dispatch -------------------------------------------------------

  /// Dispatch the Ready derivation to an eligible executor slot.
  // Mirrors: dispatch.rs::dispatch_ready (the backoff defer) +
  //          assignment.rs::hard_filter (failed_builders exclusion) +
  //          dispatch.rs::assign_to_worker (backoff_until = None,
  //          running_build = Some).
  action dispatchTo(w: str): bool = all {
    dStatus == DReady,
    backoffElapsed,
    physAttempts < MAX_ATTEMPTS,
    placeable.contains(w),
    dStatus' = DRunning,
    attempt' = Running(w),
    execs' = execs.put(w, { ...execs.get(w), runningOurs: true }),
    live' = { ...live, backoffUntil: NoTime },
    val applied = specApply(spec, FDispatched(now), eligibleFleet)
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
    },
    physAttempts' = physAttempts + 1,
    now' = now, poisonSrc' = poisonSrc, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = noObs,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// E9 -- the dispatch-time fleet-exhaust backstop: no placeable
  /// executor and every eligible one has already failed this derivation.
  /// Poisons without a failure event (no fold event -- poisonSrc records
  /// the scoping). The empty eligible fleet defers instead (the action
  /// is disabled), per sched.dispatch.fleet-exhaust+3.
  // Mirrors: dispatch.rs (find_executor == None arm) +
  //          failed_builders_exhausts_fleet + poison_and_cascade.
  action dispatchFleetExhaust = all {
    dStatus == DReady,
    backoffElapsed,
    placeable == Set(),
    exhaustsFleet(live.failedBuilders, eligibleFleet),
    dStatus' = DPoisoned,
    poisonSrc' = DispatchPoison,
    live' = { ...live, poisonedAt: At(now) },
    lastPoisonReason' = RFleetExhausted,
    // persist_poisoned (best-effort; not fault-injected here -- the
    // dispatch-time poison is not a calibration subject)
    pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
    depStatus' = DepFailed,
    everFailureTerminal' = true,
    spec' = spec, specVerdict' = specVerdict,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft,
    last' = { ...noObs, kind: OE9Dispatch, terminal: true },
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7b. physical attempt ends -----------------------------------------

  /// The running build ends worker-side with a CompletionReport of the
  /// given class in flight. `dual` marks the deadline overrun that the
  /// Job controller also observes (activeDeadlineSeconds fired around
  /// the same time the worker got its TimedOut report out) -- the
  /// dual-observation shape sched.retry.no-double-count is about.
  action buildEndsReported(w: str, class: ReportClass, dual: bool): bool = all {
    ENABLE_WORKER,
    attempt == Running(w),
    dual implies (class == RcTimeout and ENABLE_PODDEATH),
    attempt' = Reported({ exec: w, class: class, dual: dual }),
    execs' = if (dual)
      execs.put(w, { ...execs.get(w), ctrl: CtrlDeadline, ctrlStale: false,
        rdForCtrl: false, deathCharges: 0 })
    else execs,
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    last' = last, lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The running build's pod dies without a worker report: the stream
  /// will close (disconnect pending) and the controller learns a
  /// termination reason it will report (OOM / DeadlineExceeded), or the
  /// death is a non-resource crash the controller has nothing useful to
  /// say about (the C2 shape).
  action buildEndsPodDeath(w: str, kind: DeathKind): bool = all {
    attempt == Running(w),
    match kind {
      | DkOom => ENABLE_PODDEATH
      | DkDeadline => ENABLE_PODDEATH
      | DkCrash => ENABLE_CRASH
    },
    attempt' = PodDead({ exec: w, kind: kind }),
    execs' = execs.put(w, { ...execs.get(w),
      ctrl: match kind { | DkOom => CtrlOom | DkDeadline => CtrlDeadline | DkCrash => NoCtrl },
      ctrlStale: false,
      rdForCtrl: false,
      deathCharges: 0 }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    last' = last, lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The running build wedges: the worker keeps heartbeating but never
  /// reports and the pod never dies -- the backstop timer's subject.
  action buildWedges(w: str): bool = all {
    ENABLE_WEDGE,
    attempt == Running(w),
    attempt' = Wedged(w),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    execs' = execs, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7c. the worker-report entry points (E1-E4) -------------------------

  /// Process the pending CompletionReport. One action covers the four
  /// completion-routing entry points; the per-class arms transcribe
  /// their handlers. The shared prologue is handle_completion's executor
  /// bookkeeping: running_build cleared, last_completed set, the slot
  /// marked draining (one-shot), all BEFORE the per-class handler runs --
  /// which is why the post-report stream close never reassigns and never
  /// inserts a recently_disconnected entry (I-188 / I-197).
  // Mirrors: completion.rs::handle_completion (bookkeeping + routing) ->
  //          handle_transient_failure (E1) / handle_infrastructure_failure
  //          (E2) / handle_permanent_failure (E3) / handle_timeout_failure
  //          (E4), with their PG mirror writes (best-effort).
  action processReport(w: str, class: ReportClass, dual: bool, promoted: bool, atCap: bool, pgLost: bool): bool = all {
    attempt == Reported({ exec: w, class: class, dual: dual }),
    // the floor oracle: promoted consumes a ladder rung; at_cap is
    // always available (the SLA fit may already have dispatched at the
    // ceiling); both only apply to the cgroup-OOM class here.
    promoted implies (class == RcInfraOom and promotionsLeft > 0),
    atCap implies class == RcInfraOom,
    not(promoted and atCap),
    (class == RcInfraOom) implies (promoted or atCap),
    // a lost-PG-write step needs fault budget
    pgLost implies pgFaultsLeft > 0,
    // the derivation is Running and assigned to the reporting executor
    // (handle_completion's status + stale-executor guards) -- structural
    // here: a pending report only exists for the in-flight attempt.
    dStatus == DRunning,

    // shared prologue: free the slot, remember the completion, drain.
    val execAfter = { ...execs.get(w), runningOurs: false, lastCompletedOurs: true, draining: true }
    val fleetAfter = EXECUTORS.filter(x => and {
      execs.get(x).registered,
      not(execs.get(x).draining),
      x != w,
    })
    match class {
      // ── E1: handle_transient_failure ──────────────────────────────
      | RcTransient =>
        val fb1 = live.failedBuilders.union(Set(w))
        val c1 = { ...live, failedBuilders: fb1, failureCount: live.failureCount + 1 }
        val reachedPoison = c1.failedBuilders.size() >= THRESHOLD
        val exhausts = exhaustsFleet(fb1, fleetAfter)
        val applied = specApply(spec, FTransient({ at: now, exec: w }), fleetAfter)
        all {
          execs' = execs.put(w, execAfter),
          attempt' = NoAttempt,
          spec' = applied.c,
          specVerdict' = applied.v,
          if (reachedPoison or exhausts) all {
            // poison_and_cascade
            dStatus' = DPoisoned,
            poisonSrc' = EventPoison,
            live' = { ...c1, poisonedAt: At(now) },
            lastPoisonReason' = if (reachedPoison) RThreshold else RFleetExhausted,
            pg' = if (pgLost) pg else { ...pg,
              failedBuilders: pg.failedBuilders.union(Set(w)),
              status: PgPoisoned, poisonedAt: At(now) },
            depStatus' = DepFailed,
            everFailureTerminal' = true,
            last' = { ...noObs, kind: OE1, chargedAny: true, terminal: true },
          } else if (live.count < MAX_RETRIES) all {
            // retry arm: backoff from the pre-increment count
            dStatus' = DReady,
            poisonSrc' = poisonSrc,
            live' = { ...c1,
              count: live.count + 1,
              backoffUntil: At(now + backoffSecs(live.count)) },
            lastPoisonReason' = lastPoisonReason,
            pg' = if (pgLost) pg else { ...pg,
              failedBuilders: pg.failedBuilders.union(Set(w)),
              retryCount: pg.retryCount + 1 },
            depStatus' = depStatus,
            everFailureTerminal' = everFailureTerminal,
            last' = { ...noObs, kind: OE1, chargedAny: true },
          } else all {
            // max_retries exhausted -> poison
            dStatus' = DPoisoned,
            poisonSrc' = EventPoison,
            live' = { ...c1, poisonedAt: At(now) },
            lastPoisonReason' = RTransientBudget,
            pg' = if (pgLost) pg else { ...pg,
              failedBuilders: pg.failedBuilders.union(Set(w)),
              status: PgPoisoned, poisonedAt: At(now) },
            depStatus' = DepFailed,
            everFailureTerminal' = true,
            last' = { ...noObs, kind: OE1, chargedAny: true, terminal: true },
          },
          promotionsLeft' = promotionsLeft,
        }

      // ── E2: handle_infrastructure_failure ─────────────────────────
      | RcInfraPutPath => infraArm(w, execAfter, fleetAfter, true, false, false, pgLost)
      | RcInfraOom => infraArm(w, execAfter, fleetAfter, promoted, atCap, promoted, pgLost)
      | RcInfraOther => infraArm(w, execAfter, fleetAfter, false, false, false, pgLost)

      // ── E3: handle_permanent_failure ──────────────────────────────
      | RcPermanent =>
        val applied = specApply(spec, FPermanent({ at: now, exec: w }), fleetAfter)
        all {
          execs' = execs.put(w, execAfter),
          attempt' = NoAttempt,
          spec' = applied.c,
          specVerdict' = applied.v,
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          live' = { ...live,
            poisonedAt: At(now),
            failedBuilders: live.failedBuilders.union(Set(w)) },
          lastPoisonReason' = RPermanent,
          pg' = if (pgLost) pg else { ...pg,
            status: PgPoisoned, poisonedAt: At(now),
            failedBuilders: pg.failedBuilders.union(Set(w)) },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OE3, chargedAny: true, terminal: true },
          promotionsLeft' = promotionsLeft,
        }

      // ── E4: handle_timeout_failure ────────────────────────────────
      | RcTimeout =>
        val applied = specApply(spec, FWorkerTimeout(now), fleetAfter)
        all {
          spec' = applied.c,
          specVerdict' = applied.v,
          attempt' = NoAttempt,
          if (live.timeoutCount < MAX_TIMEOUT) all {
            dStatus' = DReady,
            poisonSrc' = poisonSrc,
            live' = { ...live, timeoutCount: live.timeoutCount + 1 },
            lastPoisonReason' = lastPoisonReason,
            pg' = pg,
            depStatus' = depStatus,
            everFailureTerminal' = everFailureTerminal,
            execs' = execs.put(w, if (dual) { ...execAfter, deathCharges: execAfter.deathCharges + 1 } else execAfter),
            last' = { ...noObs, kind: OE4, chargedAny: true },
          } else all {
            // terminal Cancelled (NOT Poisoned) + cascade
            dStatus' = DCancelled,
            poisonSrc' = poisonSrc,
            live' = live,
            lastPoisonReason' = lastPoisonReason,
            pg' = if (pgLost) pg else { ...pg, status: PgCancelled },
            depStatus' = DepFailed,
            everFailureTerminal' = true,
            execs' = execs.put(w, if (dual) { ...execAfter, deathCharges: execAfter.deathCharges + 1 } else execAfter),
            last' = { ...noObs, kind: OE4, terminal: true },
          },
          promotionsLeft' = promotionsLeft,
        }
    },

    pgFaultsLeft' = if (pgLost) pgFaultsLeft - 1 else pgFaultsLeft,
    everFailedBuilders' = if (class == RcTransient or class == RcPermanent)
      everFailedBuilders.union(Set(w)) else everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  /// E2's body, shared by the three worker-reported infra classes:
  /// the exempt block (increment + cap, no early return under cap), the
  /// I-127 window reset (gated only on the event's own at-cap outcome --
  /// the exempt fall-through), then the non-exempt cap check and charge.
  // Mirrors: completion.rs::handle_infrastructure_failure.
  action infraArm(w: str, execAfter: ExecRec, fleetAfter: Set[str], exempt: bool, atCap: bool, consumesPromotion: bool, pgLost: bool): bool = all {
    val applied = specApply(spec, FInfra({ at: now, exempt: exempt, atCap: atCap }), fleetAfter)
    val cExempt = if (exempt) { ...live, exemptInfraCount: live.exemptInfraCount + 1 } else live
    val exemptCapHit = exempt and cExempt.exemptInfraCount >= MAX_EXEMPT
    val windowElapsed = and {
      not(atCap),
      match cExempt.lastInfraAt { | NoTime => false | At(t) => now - t > INFRA_WINDOW },
    }
    val cReset = if (windowElapsed and not(exemptCapHit)) { ...cExempt, infraCount: 0 } else cExempt
    val infraCapHit = not(exempt) and cReset.infraCount >= MAX_INFRA
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = if (consumesPromotion) promotionsLeft - 1 else promotionsLeft,
      if (exemptCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...cExempt, poisonedAt: At(now) },
        lastPoisonReason' = RExemptInfraBudget,
        pg' = if (pgLost) pg else { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, chargedAny: true, terminal: true,
          promoted: consumesPromotion, atCap: atCap },
      } else if (infraCapHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...cReset, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = if (pgLost) pg else { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE2, terminal: true, windowReset: windowElapsed,
          promoted: consumesPromotion, atCap: atCap },
      } else all {
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = if (exempt) cReset
          else { ...cReset, infraCount: cReset.infraCount + 1, lastInfraAt: At(now) },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE2, chargedAny: true, windowReset: windowElapsed,
          exemptFallthrough: windowElapsed and exempt,
          promoted: consumesPromotion, atCap: atCap },
      },
    },
  }

  // --- 7d. the disconnect entry point (E5) -------------------------------

  /// Process the dead pod's stream close (or the equivalent heartbeat
  /// timeout): remove the slot from the executor map, record the
  /// mid-build death for the controller's follow-up report (unless a
  /// race-ahead report already handled it -- last_completed), and
  /// reassign: re-check the existing poison state over failures recorded
  /// by other paths, else reset to Ready. A bare disconnect charges
  /// nothing (contradiction C2: the spec says it should count; the code
  /// does not, and this model encodes the code).
  // Mirrors: executor.rs::handle_executor_disconnected +
  //          reassign_derivations (E5).
  action processDisconnect(w: str): bool = all {
    match attempt { | PodDead(p) => p.exec == w | _ => false },
    val e = execs.get(w)
    val insertRd = not(e.lastCompletedOurs)
    val stillRunning = dStatus == DRunning
    val poisonNow = stillRunning and thresholdReached(live)
    val applied = specApply(spec, FDisconnect(now), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e,
        registered: false, draining: false, runningOurs: false,
        rd: if (insertRd) At(now) else e.rd,
        rdForCtrl: if (insertRd) true else e.rdForCtrl }),
      attempt' = NoAttempt,
      spec' = if (stillRunning) applied.c else spec,
      specVerdict' = if (stillRunning) applied.v else specVerdict,
      if (poisonNow) all {
        // 3 prior recorded failures + this disconnect: poison instead of
        // a 4th dispatch (poison_and_cascade from reassign_derivations).
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RThreshold,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE5, terminal: true },
      } else if (stillRunning) all {
        // reset_to_ready + persist Ready: charge nothing.
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE5 },
      } else all {
        // The derivation already left Running (a race-ahead report
        // poisoned it at the cap): executor bookkeeping only; the
        // reassign skips the node (reset_to_ready rejects).
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE5 },
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  /// A pending CompletionReport for a derivation that is no longer
  /// Running (a late controller report from a previous death poisoned it
  /// while this attempt was in flight): handle_completion's executor
  /// bookkeeping still runs, the per-class handler is skipped by the
  /// status guard, nothing is charged and no fold event is observed.
  // Mirrors: completion.rs::handle_completion (the not-Assigned/Running
  //          early return after the capacity-free hoist).
  action processReportStale(w: str): bool = all {
    match attempt { | Reported(r) => r.exec == w | _ => false },
    dStatus != DRunning,
    execs' = execs.put(w, { ...execs.get(w),
      runningOurs: false, lastCompletedOurs: true, draining: true }),
    attempt' = NoAttempt,
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    last' = noObs, lastPoisonReason' = lastPoisonReason,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  // --- 7e. the controller-report entry points (E6/E7) ---------------------

  /// E6, the normal (post-disconnect) order: the controller's
  /// OOM/DiskPressure report is correlated back to the derivation
  /// through the recently_disconnected entry (first-report-wins
  /// removal). At the floor ceiling this path owns the infra cap check
  /// and the increment; the increment does NOT stamp the window anchor
  /// (DIVERGENCE D2). A floor-promoted report charges nothing
  /// (DIVERGENCE D3 / contradiction C3). The derivation may already be
  /// running its next attempt (the late installment) -- the status guard
  /// admits Ready/Assigned/Running, so the charge lands regardless.
  // Mirrors: executor.rs::handle_executor_termination (the rd-entry arm).
  action ctrlDeliverOomLate(w: str, promoted: bool, atCap: bool): bool = all {
    execs.get(w).ctrl == CtrlOom,
    execs.get(w).rd != NoTime,
    // the entry and the report name the same pod (a report for a newer
    // death never matches an older incarnation's entry, and vice versa)
    execs.get(w).rdForCtrl,
    promoted implies promotionsLeft > 0,
    not(promoted and atCap),
    promoted or atCap,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val charges = statusOk and atCap
    val capHit = charges and live.infraCount >= MAX_INFRA
    val applied = specApply(spec,
      FControllerTermination({ at: now, promoted: promoted, atCap: atCap }), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e, rd: NoTime, ctrl: NoCtrl,
        deathCharges: if (charges) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      promotionsLeft' = if (promoted) promotionsLeft - 1 else promotionsLeft,
      if (capHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE6, terminal: true, atCap: atCap,
          promoted: promoted, lateInstallment: true },
      } else if (charges) all {
        // DIVERGENCE D2: infra_count incremented, anchor NOT stamped.
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, chargedAny: true, atCap: atCap,
          promoted: promoted, lateInstallment: true },
      } else all {
        // DIVERGENCE D3: the promoted arm charges nothing (or the
        // status guard dropped the report).
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, atCap: atCap, promoted: promoted,
          lateInstallment: true },
      },
    },
    attempt' = attempt,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// E6, the race-ahead order: the report lands before the disconnect.
  /// The drv is resolved through the still-live executor's running_build
  /// and last_completed is set so the imminent disconnect does not
  /// insert a recently_disconnected entry (the I-197/ee9302b86 dedup).
  // Mirrors: executor.rs::handle_executor_termination (the race-ahead arm).
  action ctrlDeliverOomRaceAhead(w: str, promoted: bool, atCap: bool): bool = all {
    match attempt { | PodDead(p) => p.exec == w and p.kind == DkOom | _ => false },
    execs.get(w).ctrl == CtrlOom,
    not(execs.get(w).ctrlStale),
    execs.get(w).rd == NoTime,
    promoted implies promotionsLeft > 0,
    not(promoted and atCap),
    promoted or atCap,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val charges = statusOk and atCap
    val capHit = charges and live.infraCount >= MAX_INFRA
    val applied = specApply(spec,
      FControllerTermination({ at: now, promoted: promoted, atCap: atCap }), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e, ctrl: NoCtrl, lastCompletedOurs: true,
        deathCharges: if (charges) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      promotionsLeft' = if (promoted) promotionsLeft - 1 else promotionsLeft,
      if (capHit) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RInfraBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE6, terminal: true, atCap: atCap,
          promoted: promoted, raceAhead: true },
      } else if (charges) all {
        // DIVERGENCE D2 again: no anchor stamp.
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, infraCount: live.infraCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, chargedAny: true, atCap: atCap,
          promoted: promoted, raceAhead: true },
      } else all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE6, atCap: atCap, promoted: promoted,
          raceAhead: true },
      },
    },
    attempt' = attempt,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// E7: the controller's DeadlineExceeded report, prefix-matched to a
  /// recently_disconnected entry (no race-ahead arm exists for this
  /// path). Charges timeout_count unconditionally under the cap; AT the
  /// cap it poisons -- DIVERGENCE D1: the worker-reported E4 produces
  /// terminal Cancelled for the same exhausted budget, so the verdict
  /// depends on which observer reported first.
  // Mirrors: executor.rs::handle_deadline_exceeded.
  action ctrlDeliverDeadline(w: str): bool = all {
    execs.get(w).ctrl == CtrlDeadline,
    execs.get(w).rd != NoTime,
    // the prefix match only ever finds pods of the reporting job: the
    // entry must be the report's own death's.
    execs.get(w).rdForCtrl,
    val e = execs.get(w)
    val statusOk = dStatus == DReady or dStatus == DRunning
    val capHit = statusOk and live.timeoutCount >= MAX_TIMEOUT
    val applied = specApply(spec, FControllerDeadlineExceeded(now), eligibleFleet)
    all {
      execs' = execs.put(w, { ...e, rd: NoTime, ctrl: NoCtrl,
        deathCharges: if (statusOk) e.deathCharges + 1 else e.deathCharges }),
      spec' = if (statusOk) applied.c else spec,
      specVerdict' = if (statusOk) applied.v else specVerdict,
      if (capHit) all {
        // DIVERGENCE D1: poison_and_cascade where E4 cancels.
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...live, poisonedAt: At(now) },
        lastPoisonReason' = RTimeoutBudget,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE7, terminal: true, lateInstallment: true },
      } else if (statusOk) all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = { ...live, timeoutCount: live.timeoutCount + 1 },
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE7, chargedAny: true, lateInstallment: true },
      } else all {
        dStatus' = dStatus,
        poisonSrc' = poisonSrc,
        live' = live,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE7, lateInstallment: true },
      },
    },
    attempt' = attempt,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  /// The controller's report never lands (controller down, JOB_TTL
  /// expired, or the rd entry was swept first): the death stays
  /// unclassified -- "this one OOM didn't promote".
  action ctrlReportLost(w: str): bool = all {
    execs.get(w).ctrl != NoCtrl,
    execs' = execs.put(w, { ...execs.get(w), ctrl: NoCtrl }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7f. the backstop entry point (E8) ----------------------------------

  /// The scheduler-side backstop timer for a wedged attempt: cancel
  /// signal (best-effort, not modeled), the slot quarantined (draining,
  /// running_build cleared), the attempt recorded into failed_builders /
  /// failure_count -- with NO PG mirror (divergence D4) -- and the
  /// reassign re-check poisons at the threshold or resets to Ready.
  // Mirrors: housekeeping.rs::tick_scan_dag + tick_process_backstop_timeouts
  //          -> reassign_derivations.
  action backstopFires(w: str): bool = all {
    ENABLE_WEDGE,
    attempt == Wedged(w),
    dStatus == DRunning,
    val fb1 = live.failedBuilders.union(Set(w))
    val c1 = { ...live, failedBuilders: fb1, failureCount: live.failureCount + 1 }
    val poisonNow = c1.failedBuilders.size() >= THRESHOLD
    val applied = specApply(spec, FBackstopTimeout({ at: now, exec: w }), eligibleFleet)
    all {
      execs' = execs.put(w, { ...execs.get(w), runningOurs: false, draining: true }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      if (poisonNow) all {
        dStatus' = DPoisoned,
        poisonSrc' = EventPoison,
        live' = { ...c1, poisonedAt: At(now) },
        lastPoisonReason' = RThreshold,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        last' = { ...noObs, kind: OE8, chargedAny: true, terminal: true },
      } else all {
        dStatus' = DReady,
        poisonSrc' = poisonSrc,
        live' = c1,
        lastPoisonReason' = lastPoisonReason,
        pg' = pg,
        depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal,
        last' = { ...noObs, kind: OE8, chargedAny: true },
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders.union(Set(w)),
  }

  // --- 7g. executor lifecycle ---------------------------------------------

  /// A spent slot is replaced by a fresh pod (one-shot semantics: the
  /// controller spawns replacements). A controller report still pending
  /// for the previous incarnation stays deliverable through its
  /// recently_disconnected entry but is marked stale so the race-ahead
  /// live-executor lookup cannot match it to the new pod (in production
  /// the old pod name misses the executor map).
  action respawnExecutor(w: str): bool = all {
    not(execs.get(w).runningOurs),
    or { not(execs.get(w).registered), execs.get(w).draining },
    execs' = execs.put(w, { ...execs.get(w),
      registered: true, draining: false, lastCompletedOurs: false,
      ctrlStale: execs.get(w).ctrl != NoCtrl }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    pgFaultsLeft' = pgFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7h. resets ----------------------------------------------------------

  /// The dag::merge resubmit reset: a retriable terminal node
  /// (Cancelled unconditionally; Poisoned iff resubmit_cycles <
  /// POISON_RESUBMIT_RETRY_LIMIT) gets a fresh per-cycle state with
  /// resubmit_cycles incremented; PG is cleared via clear_poison_batch
  /// (best-effort -- a lost write leaves the stale row for recovery).
  /// The dependent's DependencyFailed is reset by the same merge.
  // Mirrors: dag/mod.rs (the resubmit-reset arm) + merge.rs
  //          clear_poison_batch + derivation.rs::is_retriable_on_resubmit.
  action resubmitReset(pgLost: bool): bool = all {
    ENABLE_RESETS,
    or {
      dStatus == DCancelled,
      and { dStatus == DPoisoned, live.resubmitCycles < RESUBMIT_LIMIT },
    },
    pgLost implies pgFaultsLeft > 0,
    val applied = specApply(spec, FResubmitReset(now), eligibleFleet)
    all {
      dStatus' = DReady,
      poisonSrc' = NoPoison,
      live' = { ...zeroCounters, resubmitCycles: live.resubmitCycles + 1 },
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = if (pgLost) pg else { ...pg, status: PgNonTerminal, poisonedAt: NoTime,
        failedBuilders: Set(), retryCount: 0, resubmitCycles: pg.resubmitCycles + 1 },
      depStatus' = DepReady,
      last' = { ...noObs, kind: OResubmit },
    },
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft,
    pgFaultsLeft' = if (pgLost) pgFaultsLeft - 1 else pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The cache-hit clear: the poisoned derivation's output turned up in
  /// the store (another tenant built it / a re-probe found it
  /// substitutable), so the failure history is moot: RetryState::clear()
  /// (backoff_until survives), transition to Completed, best-effort PG
  /// poison clear.
  // Mirrors: merge.rs::apply_cached_hits / dispatch.rs spawn_substitute
  //          reprobe lane (the Poisoned arm) + db::clear_poison.
  action cacheHitClear(pgLost: bool): bool = all {
    ENABLE_RESETS,
    dStatus == DPoisoned,
    pgLost implies pgFaultsLeft > 0,
    val applied = specApply(spec, FCacheHitClear(now), eligibleFleet)
    all {
      dStatus' = DCompleted,
      poisonSrc' = NoPoison,
      live' = clearForCacheHit(live),
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = if (pgLost) pg else { status: PgCompleted, retryCount: 0,
        failedBuilders: Set(), resubmitCycles: 0, poisonedAt: NoTime },
      last' = { ...noObs, kind: OCacheHit },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft,
    pgFaultsLeft' = if (pgLost) pgFaultsLeft - 1 else pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The poison TTL expiry (the admin ClearPoison path is identical
  /// minus the time gate and is not modeled separately): PG is cleared
  /// FIRST (a failed clear aborts -- equivalently, the action does not
  /// fire), then the node is removed from the DAG; the next merge would
  /// re-insert it fresh (not modeled -- DAbsent is a sink).
  // Mirrors: housekeeping.rs::tick_process_expired_poisons +
  //          db::clear_poison (the full reset).
  action poisonTtlExpires = all {
    ENABLE_RESETS,
    dStatus == DPoisoned,
    poisonTtlElapsed,
    val applied = specApply(spec, FPoisonCleared(now), eligibleFleet)
    all {
      dStatus' = DAbsent,
      poisonSrc' = NoPoison,
      live' = zeroCounters,
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = freshPg,
      last' = { ...noObs, kind: OTtlExpire },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, pgFaultsLeft' = pgFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7i. leader failover -------------------------------------------------

  /// Leader failover: the in-memory world (executor map, dedup state,
  /// in-flight attempt and observations, the RAM-only counters) is lost;
  /// the new leader rebuilds the derivation from its PG row -- the
  /// documented selective forgiveness -- and the orphan reconcile
  /// re-runs the threshold check over the recovered counters. The fold
  /// ghost is re-seeded alongside (see the header's encoding decisions).
  // Mirrors: actor/mod.rs LeaderLost wipe + recovery.rs
  //          (load_nonterminal_derivations -> from_recovery_row,
  //          poisoned rows -> from_poisoned_row, expired-poison clear,
  //          reset_orphan_to_ready's poison re-check).
  action leaderFailover = all {
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
    val expiredInDowntime = and {
      pg.status == PgPoisoned,
      match pg.poisonedAt { | NoTime => false | At(t) => now - t > POISON_TTL },
    }
    val projNT = recoveryProjectionNonTerminal(pg)
    val projP = recoveryProjectionPoisoned(pg)
    val orphanPoisons = pg.status == PgNonTerminal and projNT.failedBuilders.size() >= THRESHOLD
    all {
      failoversLeft' = failoversLeft - 1,
      execs' = EXECUTORS.mapBy(w => freshExec),
      attempt' = NoAttempt,
      justFailedOver' = true,
      last' = { ...noObs, kind: OFailover },
      if (gone) all {
        // A derivation no longer part of any active build (TTL-cleared
        // or completed) is not reloaded; nothing to recover.
        dStatus' = dStatus, live' = live, spec' = spec, specVerdict' = specVerdict,
        pg' = pg, poisonSrc' = poisonSrc, depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal, lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned and expiredInDowntime) all {
        // f9adf3c76: the TTL elapsed while no leader was watching --
        // clear in PG, do not reload.
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = freshPg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned) all {
        // from_poisoned_row: resubmit_cycles / failed_builders /
        // poisoned_at recovered, failure_count derived, the rest
        // defaulted (note: NOT retry_count -- the poisoned-row
        // constructor never reads it). The row may even resurrect a
        // poison the previous tenure had already reset in memory (a
        // resubmit whose clear_poison_batch write was lost): the ghost
        // poison-source falls back to EventPoison and the recovery-time
        // failed-dep cascade re-seeds the dependent
        // (sched.recovery.failed-dep-cascade).
        dStatus' = DPoisoned, live' = projP, spec' = projP,
        specVerdict' = VPoison(RThreshold), pg' = pg,
        poisonSrc' = if (poisonSrc == NoPoison) EventPoison else poisonSrc,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgNonTerminal and orphanPoisons) all {
        // reset_orphan_to_ready's poison re-check over the recovered
        // counters: the projection already crosses the threshold, so the
        // reconcile poisons instead of re-dispatching.
        dStatus' = DPoisoned,
        live' = { ...projNT, poisonedAt: At(now) },
        spec' = projNT,
        specVerdict' = VRequeue,
        pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
        poisonSrc' = RecoveryPoison,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = RThreshold,
      } else if (pg.status == PgNonTerminal) all {
        // from_recovery_row + the orphan reconcile's reset_to_ready.
        dStatus' = DReady, live' = projNT, spec' = projNT,
        specVerdict' = VRequeue, pg' = pg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      } else all {
        // PgCancelled / PgCompleted: terminal rows are not reloaded.
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = pg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    pgFaultsLeft' = pgFaultsLeft, everFailedBuilders' = everFailedBuilders,
  }

  // --- 8. init ------------------------------------------------------------

  action init = all {
    now' = 0,
    dStatus' = DReady,
    live' = zeroCounters,
    poisonSrc' = NoPoison,
    spec' = zeroCounters,
    specVerdict' = VRequeue,
    pg' = freshPg,
    depStatus' = DepReady,
    everFailureTerminal' = false,
    attempt' = NoAttempt,
    execs' = EXECUTORS.mapBy(w => freshExec),
    promotionsLeft' = MAX_PROMOTIONS,
    physAttempts' = 0,
    failoversLeft' = MAX_FAILOVERS,
    pgFaultsLeft' = PG_FAULTS,
    last' = noObs,
    lastPoisonReason' = RThreshold,
    justFailedOver' = false,
    everFailedBuilders' = Set(),
  }

  // --- 9. step --------------------------------------------------------------
  // Every disjunct is a named action with its nondet choices bound
  // directly inside it.

  pure val REPORT_CLASSES = Set(RcTransient, RcInfraPutPath, RcInfraOom, RcInfraOther, RcPermanent, RcTimeout)
  pure val DEATH_KINDS = Set(DkOom, DkDeadline, DkCrash)
  pure val BOOLS = Set(true, false)

  action dispatchToAny = { nondet w = EXECUTORS.oneOf() dispatchTo(w) }
  action buildEndsReportedAny = {
    nondet w = EXECUTORS.oneOf()
    nondet class = REPORT_CLASSES.oneOf()
    nondet dual = BOOLS.oneOf()
    buildEndsReported(w, class, dual)
  }
  action buildEndsPodDeathAny = {
    nondet w = EXECUTORS.oneOf()
    nondet kind = DEATH_KINDS.oneOf()
    buildEndsPodDeath(w, kind)
  }
  action buildWedgesAny = { nondet w = EXECUTORS.oneOf() buildWedges(w) }
  action processReportAny = {
    nondet w = EXECUTORS.oneOf()
    nondet class = REPORT_CLASSES.oneOf()
    nondet dual = BOOLS.oneOf()
    nondet promoted = BOOLS.oneOf()
    nondet atCap = BOOLS.oneOf()
    nondet pgLost = BOOLS.oneOf()
    processReport(w, class, dual, promoted, atCap, pgLost)
  }
  action processReportStaleAny = { nondet w = EXECUTORS.oneOf() processReportStale(w) }
  action processDisconnectAny = { nondet w = EXECUTORS.oneOf() processDisconnect(w) }
  action ctrlDeliverOomLateAny = {
    nondet w = EXECUTORS.oneOf()
    nondet promoted = BOOLS.oneOf()
    nondet atCap = BOOLS.oneOf()
    ctrlDeliverOomLate(w, promoted, atCap)
  }
  action ctrlDeliverOomRaceAheadAny = {
    nondet w = EXECUTORS.oneOf()
    nondet promoted = BOOLS.oneOf()
    nondet atCap = BOOLS.oneOf()
    ctrlDeliverOomRaceAhead(w, promoted, atCap)
  }
  action ctrlDeliverDeadlineAny = { nondet w = EXECUTORS.oneOf() ctrlDeliverDeadline(w) }
  action ctrlReportLostAny = { nondet w = EXECUTORS.oneOf() ctrlReportLost(w) }
  action backstopFiresAny = { nondet w = EXECUTORS.oneOf() backstopFires(w) }
  action respawnExecutorAny = { nondet w = EXECUTORS.oneOf() respawnExecutor(w) }
  action resubmitResetAny = { nondet pgLost = BOOLS.oneOf() resubmitReset(pgLost) }
  action cacheHitClearAny = { nondet pgLost = BOOLS.oneOf() cacheHitClear(pgLost) }

  action step = any {
    tick,
    dispatchToAny,
    dispatchFleetExhaust,
    buildEndsReportedAny,
    buildEndsPodDeathAny,
    buildWedgesAny,
    processReportAny,
    processReportStaleAny,
    processDisconnectAny,
    ctrlDeliverOomLateAny,
    ctrlDeliverOomRaceAheadAny,
    ctrlDeliverDeadlineAny,
    ctrlReportLostAny,
    backstopFiresAny,
    respawnExecutorAny,
    resubmitResetAny,
    cacheHitClearAny,
    poisonTtlExpires,
    leaderFailover,
  }
}

// ===========================================================================
// THE WORKER-CHANNEL REGIME: every failure is worker-reported (E1-E4), two
// executor slots, resets enabled, no pod deaths, no controller channel, no
// backstop, no failover, no PG faults. This is the regime where the live
// counters and the reference fold share the whole event alphabet, so
// countersRefineHistory and verdictMatchesFold are expected to HOLD: it
// validates that the scattered per-entry-point increments (with their
// per-counter fencepost conventions, the window reset and its exempt
// fall-through, the backoff arming, the threshold/fleet/cap ordering)
// really are the fold -- and it carries the placement/threshold scenarios
// that need a second slot.
// ===========================================================================
module retryPolicyWorker {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1", "w2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 6,
    ATTEMPT_BOUND = 6,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true
  ).*

  /// The transient budget mechanics: the first failure charges count /
  /// failure_count / failed_builders and arms the backoff (computed from
  /// the pre-increment count), the placement exclusion sends the retry to
  /// the other slot, the second distinct failure trips the poison
  /// threshold (under default-shaped constants the distinct-worker
  /// threshold always fires before the per-cycle count cap, because
  /// hard_filter never lets the same worker fail the same derivation
  /// twice in a cycle), the cascade reaches the dependent, and the
  /// resubmit reset restores the per-cycle budget while counting the
  /// cycle.
  run transientBudgetRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTransient, false))
      .then(processReport("w1", RcTransient, false, false, false, false))
      .expect(all {
        live.count == 1, live.failureCount == 1, live.backoffUntil == At(1),
        live.failedBuilders == Set("w1"), pg.failedBuilders == Set("w1"),
        countersRefineHistory, verdictMatchesFold, dStatus == DReady,
        not(placeable.contains("w1")),
      })
      .then(tick)
      .then(dispatchTo("w2"))
      .then(buildEndsReported("w2", RcTransient, false))
      .then(processReport("w2", RcTransient, false, false, false, false))
      .expect(all {
        dStatus == DPoisoned, lastPoisonReason == RThreshold,
        depStatus == DepFailed, countersRefineHistory, verdictMatchesFold,
        cascadeReachesExactlyTheDependents, durableMirrorsCharges,
      })
      .then(resubmitReset(false))
      .expect(all {
        dStatus == DReady, live.resubmitCycles == 1, live.count == 0,
        live.failedBuilders == Set(), depStatus == DepReady,
        countersRefineHistory, recoveryNeverFabricatesFailures,
      })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTransient, false))
      .then(processReport("w1", RcTransient, false, false, false, false))
      .expect(all {
        dStatus == DReady, live.count == 1, live.resubmitCycles == 1,
        countersRefineHistory,
      })

  /// The distinct-worker poison threshold: one failure on each slot and
  /// the second one trips the threshold before the count cap is reached.
  run thresholdPoisonRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTransient, false))
      .then(processReport("w1", RcTransient, false, false, false, false))
      .then(tick)
      .then(dispatchTo("w2"))
      .then(buildEndsReported("w2", RcTransient, false))
      .then(processReport("w2", RcTransient, false, false, false, false))
      .expect(all {
        dStatus == DPoisoned, lastPoisonReason == RThreshold,
        live.failedBuilders == Set("w1", "w2"),
        countersRefineHistory, verdictMatchesFold, durableMirrorsCharges,
      })

  /// The I-127 window reset, including the as-built exempt fall-through:
  /// a counted infra failure anchors the window, an exempt
  /// (CONCURRENT_PUTPATH) failure past the window also zeroes the counted
  /// budget while charging only the exemption budget, and the fold
  /// reproduces both.
  run infraWindowExemptFallthroughRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcInfraOther, false))
      .then(processReport("w1", RcInfraOther, false, false, false, false))
      .expect(all { live.infraCount == 1, live.lastInfraAt == At(0), countersRefineHistory })
      .then(tick)
      .then(tick)
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcInfraPutPath, false))
      .then(processReport("w1", RcInfraPutPath, false, false, false, false))
      .expect(all {
        live.infraCount == 0,
        live.exemptInfraCount == 1,
        live.lastInfraAt == At(0),
        last.windowReset, last.exemptFallthrough,
        countersRefineHistory,
      })

  /// The worker-reported timeout budget: terminal Cancelled (not
  /// Poisoned) at the cap, immediately resubmit-retriable.
  run workerTimeoutCancelRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTimeout, false))
      .then(processReport("w1", RcTimeout, false, false, false, false))
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTimeout, false))
      .then(processReport("w1", RcTimeout, false, false, false, false))
      .expect(all { live.timeoutCount == 2, dStatus == DReady })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTimeout, false))
      .then(processReport("w1", RcTimeout, false, false, false, false))
      .expect(all {
        dStatus == DCancelled, specVerdict == VCancel,
        verdictMatchesFold, countersRefineHistory,
        depStatus == DepFailed,
      })
      .then(resubmitReset(false))
      .expect(dStatus == DReady and live.resubmitCycles == 1)
}

// ===========================================================================
// THE DUAL-CHANNEL REGIME: pod deaths, the controller report channel (with
// race-ahead, late-installment, re-dispatch interleavings and report loss),
// the wedge/backstop, the no-report crash, and the worker channel, on one
// executor slot. This is the regime that carries the pre-registered
// expected falsifications: verdictMatchesFold falsifies on the D1 history
// (the timeout cap exhausted via E7 lands as Poisoned), countersRefineHistory
// falsifies on D2/D3 histories (the at-cap controller increment without the
// window anchor; the promoted controller termination that charges nothing),
// and durableMirrorsCharges falsifies on D4 (the backstop charge that never
// reaches PG). The dedup invariant noDoubleCount and the per-event charge
// discipline are expected to HOLD.
// ===========================================================================
module retryPolicyDual {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 6,
    ATTEMPT_BOUND = 6,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = true,
    ENABLE_RESETS = true
  ).*

  /// D1 reproducer: the same exhausted timeout budget that E4 turns into
  /// terminal Cancelled is turned into Poisoned by E7 when the controller
  /// observes the deadline overruns -- the verdict depends on the channel.
  run d1ControllerTimeoutCapPoisonRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkDeadline))
      .then(processDisconnect("w1"))
      .then(ctrlDeliverDeadline("w1"))
      .expect(all { live.timeoutCount == 1, countersRefineHistory, verdictMatchesFold })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkDeadline))
      .then(processDisconnect("w1"))
      .then(ctrlDeliverDeadline("w1"))
      .expect(all { live.timeoutCount == 2, countersRefineHistory, verdictMatchesFold })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkDeadline))
      .then(processDisconnect("w1"))
      .then(ctrlDeliverDeadline("w1"))
      .expect(all {
        // the cap-hitting observation arrives on the controller channel:
        // the as-built E7 poisons (24 h TTL, bounded resubmit) where the
        // fold -- and the worker-reported E4 -- produce Cancel.
        dStatus == DPoisoned,
        lastPoisonReason == RTimeoutBudget,
        specVerdict == VCancel,
        not(verdictMatchesFold),
        not(countersRefineHistory),
        noDoubleCount,
      })

  /// D2 reproducer: an at-cap controller-reported OOM charges infra_count
  /// without stamping the window anchor; the fold stamps it.
  run d2AtCapAnchorRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkOom))
      .then(processDisconnect("w1"))
      .then(ctrlDeliverOomLate("w1", false, true))
      .expect(all {
        live.infraCount == 1,
        live.lastInfraAt == NoTime,
        spec.infraCount == 1,
        spec.lastInfraAt == At(0),
        not(countersRefineHistory),
        last.lateInstallment,
        noDoubleCount,
        verdictMatchesFold,
      })

  /// D3 reproducer: a floor-promoted controller-reported OOM charges
  /// nothing; the fold charges exempt_infra_count per
  /// sched.retry.exempt-infra-cap.
  run d3PromotedChargesNothingRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkOom))
      .then(processDisconnect("w1"))
      .then(ctrlDeliverOomLate("w1", true, false))
      .expect(all {
        live.exemptInfraCount == 0,
        spec.exemptInfraCount == 1,
        not(countersRefineHistory),
        verdictMatchesFold,
        noDoubleCount,
      })

  /// D4 reproducer: the backstop's failed_builders/failure_count charge
  /// has no PG mirror, so the durable view permanently misses it.
  run d4BackstopUnmirroredRun =
    init
      .then(dispatchTo("w1"))
      .then(buildWedges("w1"))
      .then(backstopFires("w1"))
      .expect(all {
        live.failedBuilders == Set("w1"),
        live.failureCount == 1,
        pg.failedBuilders == Set(),
        not(durableMirrorsCharges),
        countersRefineHistory,
        dStatus == DReady,
      })

  /// The dedup under the dual observation of one death: the race-ahead
  /// report charges, sets last_completed so the disconnect does not
  /// insert a recently_disconnected entry, and the late re-report finds
  /// nothing -- exactly one counted accounting event for one death.
  run dualObservationDedupRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkOom))
      .then(ctrlDeliverOomRaceAhead("w1", false, true))
      .expect(all { live.infraCount == 1, last.raceAhead, noDoubleCount })
      .then(processDisconnect("w1"))
      .expect(all {
        execs.get("w1").rd == NoTime,
        live.infraCount == 1,
        noDoubleCount,
        dStatus == DReady,
      })

  /// The late installment landing after the derivation was already
  /// re-dispatched: the charge still lands on the derivation (the status
  /// guard admits Running), the next attempt keeps running.
  run lateInstallmentAfterRedispatchRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkOom))
      .then(processDisconnect("w1"))
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(ctrlDeliverOomLate("w1", false, true))
      .expect(all {
        dStatus == DRunning,
        live.infraCount == 1,
        last.lateInstallment,
        noDoubleCount,
        not(countersRefineHistory), // D2: the anchor again
      })
}

// ===========================================================================
// THE CRASH REGIME: the C2 no-report hard-crash loop in isolation. The only
// failure the executor ever produces is a daemon crash with no
// CompletionReport and no usable controller reason; every cycle resets the
// derivation to Ready and charges nothing, so the boundedness clause of
// sched.retry.attempts-bounded+2 is falsified: physAttempts exceeds the
// bound the poison threshold would justify if disconnects counted (the
// spec's "executor disconnect DOES count" sentence -- contradiction C2).
// ===========================================================================
module retryPolicyCrash {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 1,
    MAX_ATTEMPTS = 5,
    ATTEMPT_BOUND = 3,
    MAX_FAILOVERS = 0,
    PG_FAULTS = 0,
    ENABLE_WORKER = false,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false
  ).*

  /// C2 reproducer: four uncounted dispatch-crash-requeue cycles exceed
  /// the threshold-derived bound without any budget moving.
  run c2CrashLoopRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkCrash))
      .then(processDisconnect("w1"))
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkCrash))
      .then(processDisconnect("w1"))
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsPodDeath("w1", DkCrash))
      .then(processDisconnect("w1"))
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .expect(all {
        physAttempts == 4,
        not(attemptsBoundedGlobal),
        live == zeroCounters,
        countersRefineHistory,
        attemptsChargedOnce,
      })
}

// ===========================================================================
// THE FAULT-PERSIST / FAILOVER REGIME: the dual-channel alphabet (minus the
// crash loop) plus one leader failover and one silently-lost best-effort PG
// mirror write. The recovery invariants are the load-bearing ones here: the
// post-failover state is the documented 4-recovered / 1-derived / 5-defaulted
// projection of whatever PG actually holds, and nothing in PG or in the
// recovered state claims a failure that never happened -- lost writes only
// ever make the durable view (and therefore the recovered state) more
// forgiving, never less.
// ===========================================================================
module retryPolicyFailover {
  import retryPolicyAsBuilt(
    EXECUTORS = Set("w1"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 5,
    ATTEMPT_BOUND = 5,
    MAX_FAILOVERS = 1,
    PG_FAULTS = 1,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = false,
    ENABLE_WEDGE = true,
    ENABLE_RESETS = true
  ).*

  /// The selective forgiveness end to end: an infra charge and a timeout
  /// charge (both RAM-only) plus a transient charge (mirrored); after the
  /// failover the mirrored counters come back, failure_count is re-derived
  /// from the persisted exclusion set, the RAM-only budgets are forgiven,
  /// and the fold ghost is re-seeded alongside the live counters.
  run failoverForgivenessRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcInfraOther, false))
      .then(processReport("w1", RcInfraOther, false, false, false, false))
      .expect(all { live.infraCount == 1, pg.retryCount == 0 })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTimeout, false))
      .then(processReport("w1", RcTimeout, false, false, false, false))
      .expect(all { live.timeoutCount == 1, live.infraCount == 1 })
      .then(respawnExecutor("w1"))
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcTransient, false))
      .then(processReport("w1", RcTransient, false, false, false, false))
      .expect(all {
        live.count == 1, live.failedBuilders == Set("w1"),
        pg.retryCount == 1, pg.failedBuilders == Set("w1"),
        live.infraCount == 1, live.timeoutCount == 1,
      })
      .then(leaderFailover)
      .expect(all {
        justFailedOver,
        dStatus == DReady,
        live.count == 1,
        live.failedBuilders == Set("w1"),
        live.failureCount == 1,
        live.infraCount == 0,
        live.timeoutCount == 0,
        live.backoffUntil == NoTime,
        recoveryIsTheDocumentedProjection,
        recoveryNeverFabricatesFailures,
        countersRefineHistory,
      })

  /// A lost persist_poisoned mirror write: the in-memory poison stands
  /// for the rest of the tenure, but the durable row never learned of it,
  /// so the failover recovers the derivation as Ready with only what the
  /// row supports -- the documented consequence of best-effort mirrors,
  /// not a fabrication (and not a violation of the projection rule).
  run lostPoisonMirrorFailoverRun =
    init
      .then(dispatchTo("w1"))
      .then(buildEndsReported("w1", RcPermanent, false))
      // the poisoning report's PG writes (persist_poisoned +
      // append_failed_worker) are silently lost
      .then(processReport("w1", RcPermanent, false, false, false, true))
      .expect(all {
        dStatus == DPoisoned,
        lastPoisonReason == RPermanent,
        pg.status == PgNonTerminal,
        pg.failedBuilders == Set(),
        pgFaultsLeft == 0,
        recoveryNeverFabricatesFailures,
      })
      .then(leaderFailover)
      .expect(all {
        dStatus == DReady,
        live == zeroCounters,
        recoveryIsTheDocumentedProjection,
        recoveryNeverFabricatesFailures,
        poisonIsTerminalUntilCleared,
        countersRefineHistory,
      })
}
