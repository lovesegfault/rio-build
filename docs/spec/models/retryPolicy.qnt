// -*- mode: Bluespec; -*-
//
// Model of the AS-BUILT rio-scheduler retry/poison/cascade machinery: the
// ten `RetryState` counters, the nine cap-check entry points (E1-E9 in
// docs/spec/models/retry-invariant-map.md), the signal-channel dedup
// (`recently_disconnected` / `last_completed` / draining), the four
// best-effort PG mirror columns, the leader-failover selective
// forgiveness, and the poison/cascade/reset lifecycle.
//
// This is the retry-formal campaign's Stage-B deliverable (Phase 0 of the
// design in retry-formal-design.md §3). The transition relation encodes
// THE CODE, not the reference fold: every divergent arm the invariant map
// catalogs (D1: E7 poisons at the timeout cap where E4 cancels; D2: E6's
// at-cap increment does not stamp the window anchor; D3: E6's promoted
// arm charges nothing; the E2 window-reset fall-through on exempt
// under-cap events; D4: E8's `failed_builders` charge never reaches PG)
// is reproduced as the code does it. The reference fold
// (rio-scheduler/src/retry_policy.rs) enters only as the specification
// ghost: a second copy of the counters advanced by the fold's own
// transition function at every observed accounting event, which
// `countersRefineHistory` and `verdictMatchesFold` compare the live state
// against. The pre-registered expected falsifications (the invariant
// map's Stage-B list) are exactly the divergences: those checks are wired
// expect-violation in nix/quint.nix until Phase 1 flips them.
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
module retryPolicy {
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
  // directly inside it. The decision-bearing entry points (dispatch,
  // E1-E9, the resets) join in the next stage; this stage wires the
  // attempt-end / channel / failover machinery only.

  pure val REPORT_CLASSES = Set(RcTransient, RcInfraPutPath, RcInfraOom, RcInfraOther, RcPermanent, RcTimeout)
  pure val DEATH_KINDS = Set(DkOom, DkDeadline, DkCrash)
  pure val BOOLS = Set(true, false)

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
  action ctrlReportLostAny = { nondet w = EXECUTORS.oneOf() ctrlReportLost(w) }
  action respawnExecutorAny = { nondet w = EXECUTORS.oneOf() respawnExecutor(w) }

  action step = any {
    tick,
    buildEndsReportedAny,
    buildEndsPodDeathAny,
    buildWedgesAny,
    ctrlReportLostAny,
    respawnExecutorAny,
    leaderFailover,
  }
}
