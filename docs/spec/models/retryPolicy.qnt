// -*- mode: Bluespec; -*-
//
// Model of the POST-COLLAPSE rio-scheduler retry/poison/cascade machinery
// (Phase 1b of the retry-formal campaign, re-encoded by Phase 1c): every
// budget verdict comes from `decide()` — the reference fold
// (rio-scheduler/src/retry_policy.rs) — over the durable attempt ledger
// (`drv_attempts`, migration 068), evaluated and persisted inside the
// site-owned appending transaction. The nine cap-check entry points
// (E1-E9 in docs/spec/models/retry-invariant-map.md) are callers of that
// one fold; the ten in-memory `RetryState` counters are a fold-derived
// cached view, not independently mutated state; the per-counter PG
// mirror columns, their writers, and the transitional mixed-era legacy
// seed that read them are gone (migration 075 -- see WHAT IS NOT
// MODELED for the retirement record).
//
// The pre-collapse (as-built) encoding was frozen at
// `retryPolicyAsBuilt.qnt` for the Stage-C calibration corpus and was
// retired with it in Phase 2 (the acceptance table in
// retry-invariant-map.md is the surviving record). The divergences that
// encoding reproduced are fixed here exactly as Phase 1b adjudicated
// them:
//
// EXECUTOR-CAMPAIGN 1c' RETIREMENT (T-1c'.6): the as-built-CHANNEL
// environment — the stream dispatch (dispatchTo), the disconnect entry
// point (E5), the recently_disconnected-correlated controller reports
// (E6/E7), the correlation-TTL establishment, the backstop (E8), the
// dispatch-time fleet-exhaust arm (E9), the wedge, the executor
// respawn, the as-built failover action and the four regime modules
// that composed them (worker / dual / crash / failover, with their
// named runs) — is retired with the stream machinery the executor
// campaign deleted. The pull-mode environment regime (retryPolicyPull,
// added at executor-campaign 1b per T-0e.3) is the wired set; the fold
// (`specApply`/decide()), the types, the invariants and the witness
// vals are unchanged, and the physical attempt-end events, the lost
// controller report, the resets, the clock and the appending-tx fault
// remain as the shared environment the pull regime composes. The
// retirement record (per-pin carriers included) lives in
// retry-invariant-map.md; the E1-E9 numbering below remains the
// historical entry-point inventory of that map, and the prose that
// describes the as-built channels is retained as the record of what
// the divergences D1-D4/C2 were adjudicated against.
//
//   - D1: the controller-observed timeout-cap exhaustion (E7) ends
//     terminal Cancelled, the same verdict as the worker-observed E4.
//   - D2: an at-cap controller termination (E6) stamps the 300 s window
//     anchor with its infra_count increment.
//   - D3: a floor-promoted controller termination (E6) charges
//     exempt_infra_count and poisons at the exempt cap.
//   - D4: the backstop's threshold charge (E8) is a durable ledger row;
//     the verdict is decided and persisted at the charging site.
//   - C2: a released attempt whose classifying report never arrives is
//     ESTABLISHED (executor_crash/unreported) by the correlation-TTL
//     sweep and charges the threshold/exclusion budget, so the no-report
//     crash loop is bounded.
//   - Failover: the new leader rebuilds the retry view from the fold over
//     the durable history — budgets, the window anchor and the placement
//     exclusion survive a leader change (sched.retry.failover-budget);
//     the selective forgiveness is gone.
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
//     the establishment of a never-classified released attempt (the
//     correlation-TTL sweep); the scheduler backstop timer (E8); the
//     dispatch-time fleet-exhaust check (E9).
//   - The dedup state that makes one death count at most once:
//     `recently_disconnected` (insert on mid-build disconnect only,
//     first-classifier-wins removal: a classifying report or the
//     establishment, whichever lands first), `last_completed` (the
//     post-completion disconnect and the race-ahead report both suppress
//     the insert), the one-shot draining flag, and the non-promoting
//     -reason early return (still a pure no-op -- it neither classifies
//     the row nor consumes the entry, so it is not an action).
//   - The appending transaction: at every collapsed entry point the
//     charge (the ledger row), the fold's verdict, and the derivation
//     status persist commit atomically; the in-memory cached view is
//     refreshed from the same fold after the commit. The model carries
//     the durable side as `pg.ledger` (the fold over committed rows) and
//     the cached side as `live`; the appending-transaction discipline is
//     why the two advance together. A failed appending transaction
//     (the `attemptTxFails` fault action, fault regime only) charges
//     nothing, persists nothing, and leaves the completion event pending
//     for bounded re-delivery -- the design's deliberate inversion of the
//     old best-effort-mirror posture. The poison-status write and the
//     ledger row can no longer be lost independently of each other; the
//     as-built "lost persist_poisoned mirror write" class is structurally
//     impossible for post-066 attempts.
//   - Leader failover: the executor map, dedup state, in-flight
//     observations and attempt are wiped; the derivation's retry view is
//     rebuilt by running the fold over the durable ledger (poisoned rows
//     keep their status and poisoned_at from the derivations row,
//     TTL-expired poison is cleared instead of reloaded). Nothing is
//     forgiven; only durable reset events refresh a budget.
//   - The resets: the dag::merge resubmit reset (bounded for Poisoned by
//     POISON_RESUBMIT_RETRY_LIMIT, unconditional for Cancelled), the
//     cache-hit clear (clears 9 of 10 counters, keeps backoff_until),
//     and the admin-clear/TTL-expiry path (PG-first, node removed) --
//     each is itself a durable ledger reset row, appended atomically with
//     the site's status persist.
//   - The abstract clock: the 300 s infra window, the poison TTL and the
//     transient backoff are scaled to small model constants with their
//     production ordering preserved (POISON_TTL > WINDOW >= backoff
//     base).
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - (Retired scope boundary, kept as a record:) the transitional
//     mixed-era legacy seed (decision P5 / design amendment A1) -- the
//     mirror columns frozen at the Phase-1b cutover and merged into the
//     fold as a floor for pre-066 histories -- was code-level only and
//     never modeled here (every modeled history starts at the ledger
//     era). Migration 073 dropped the columns and the seed machinery,
//     so the boundary is now moot rather than priced: the code matches
//     what this model always assumed.
//   - The recovery-time verdict enforcement (T-1b.12b) as a separate
//     arm: at model resolution the appending transaction is atomic, so a
//     committed at-budget history always has its verdict persisted with
//     it and the "crash between the attempt append and the status
//     persist" window is unconstructible. The code-level enforcement
//     (recovery re-runs decide() and acts on a Poison verdict) is
//     covered by the rio-scheduler recovery tests.
//   - The establishment's 60 s correlation TTL: the establishment action
//     is enabled as soon as the released attempt has no deliverable
//     classifying report (clock-decoupled). This over-approximates
//     production timing (production establishes only at TTL expiry) and
//     is conservative for the safety invariants; the classification
//     window the TTL exists to protect is preserved by the
//     no-deliverable-report precondition.
//   - The un-established re-dispatch window is bounded by identity
//     freshness: an executor slot whose previous death is still awaiting
//     establishment is not placeable for this derivation (production
//     replacement pods are fresh identities -- the crashed identity never
//     re-registers, so re-dispatching the same slot while its death is
//     unresolved would let one slot absorb several uncharged attempts
//     that production spreads across distinct, eventually-each-charged
//     identities). A slot whose death has a deliverable controller
//     report pending IS placeable (the late-installment /
//     re-dispatch-before-classification interleaving stays reachable).
//     This is what made `attemptsBoundedGlobal` a meaningful HOLD in
//     the retired as-built crash regime: the bound is the budget plus
//     the in-flight window, not the attempt-opening action's own
//     MAX_ATTEMPTS ceiling.
//   - The resource-floor ladder (G6). The floor enters only as the
//     {promoted, at_cap} outcome each OOM-class event consumes, with a
//     single bounded promotion budget standing in for the ladder's
//     monotone doubling. The cold-start {false,false} outcome for
//     OOM-class events is not generated. The ladder's own invariants
//     stay with floor.rs's unit tests.
//   - The substitution-failure path, the cancel paths (admin cancel,
//     per-build timeout, orphan-watcher), and build-level bookkeeping
//     (keep_going, summaries, events): out of the fold's scope per the
//     invariant map ("Out of scope" section); they consult no retry
//     counter and charge no budget.
//   - Executor identity is a reusable pod SLOT, not a fresh pod name per
//     attempt. A slot respawns (fresh registration, cleared
//     last_completed/draining) only once the controller has nothing
//     pending to report about its previous incarnation; placement
//     additionally waits for establishment (see the identity-freshness
//     bullet above). The poison threshold is scaled to the slot count so
//     every threshold path stays reachable.
//   - Static eligibility (kind/system/features) is uniform across the
//     executor slots: eligibleFleet implements only the
//     registered/non-draining half of the eligibility/draining
//     predicate.
//   - Stream epochs (the I-056a stale-disconnect guard), heartbeat
//     bookkeeping, the SLA estimator, and the backstop's wall-clock
//     threshold (the backstop fires whenever an attempt is wedged).
//   - Worker reports racing ahead of their own stream close are not
//     reordered: a successfully-sent CompletionReport is processed
//     before the disconnect that follows it (gRPC stream ordering).
//   - The non-promoting controller termination report (Completed /
//     Error / EvictedOther): it returns before classifying the row or
//     touching any dedup state (the row-level conservation T-1b.9 keeps),
//     so it is a stuttering step and is omitted from the alphabet.
//
// ENCODING DECISIONS
//   - The fold (`specApply`, a transcription of retry_policy.rs::apply)
//     is now BOTH the specification oracle and the decision arithmetic
//     the entry points execute -- that identity is the point of the
//     collapse. Every accounting action advances `live` (the cached
//     view), `pg.ledger` (the durable fold) and the `spec` ghost with
//     the same `specApply` application; `countersRefineHistory`
//     (live == spec) and `durableMirrorsCharges` (live's exclusion set
//     is contained in the durable one) are therefore true by
//     construction and are kept as cheap regression tripwires against
//     encoding drift, not as discovery instruments.
//   - The fleet-exhaust arm: production `decide()` never sees the live
//     fleet (the in-history check runs against an empty fleet) and the
//     call sites consume the exclusion set through `placeable()`. The
//     model's `specApply` is invoked with the live eligible fleet at the
//     observation, which makes the fold's verdict equal to
//     "decide() + the site's placeable() check" in one step -- the same
//     observable outcome with one fewer moving part.
//   - Dispatch-time poisons (E9) have no fold event -- the fold's event
//     alphabet is the failure-event alphabet and the `fleet_exhaust`
//     marker row is a fold no-op. They are tracked via poisonSrc and
//     excluded from the poisoned_at refinement comparison; their own
//     correctness is the placement invariant's subject.
//   - `backoff_until` is pacing, not a budget: the dispatch-time clear
//     (`assign_to_worker`) has no ledger row, so `live.backoffUntil` and
//     `pg.ledger.backoffUntil` legitimately diverge between a dispatch
//     and the next failure event, and recovery adopts the fold's
//     deterministic deadline. The refinement invariants compare live
//     against the `spec` ghost (which also observes the dispatch), not
//     against the ledger, exactly for this field.
//   - The poison threshold uses the production distinct-workers mode;
//     THRESHOLD is scaled to the executor-slot count so it is reachable.
//
// SUBSTITUTION-REPLACEMENT PHASE A (T-5.1): the model additionally
// carries the materialization attempt class — the kind partition the
// substitution-replacement design (§2.5/§9.2, OQ1 amendments 1-2) adds
// to the attempt ledger. Materialization attempts (store-replica
// executed) charge their own bounded budget, are invisible to every
// build budget, and never poison; the establishment sweep charges a
// crashed materialization attempt as materialization_infra, never
// executor_crash. Sections 11/11b/11c hold the bounded counters, the two
// partition invariants (materializationNeverPoisons,
// materializationInvisibleToBuildBudgets), the crash-channel witness and
// the three materialization actions; pullStep composes them behind the
// ENABLE_MATERIALIZATION regime switch. The pull-mode environment
// definitions (section 10) moved verbatim into this core module from the
// former standalone retryPolicyPull module so that both regime modules —
// retryPolicyPull (materialization dormant, the bit-identical pre-Phase-A
// state space) and retryPolicyPullMat (the materialization-coexistence
// regime) — instantiate one set of definitions; no action text changed
// in that move.
//
// The invariants, their witnesses and the check wiring are documented at
// their definitions and in nix/quint.nix. Measured state counts, depths
// and wall-clocks live in the introducing commits' messages and the CI
// checks' transcripts, never in this file.
module retryPolicy {
  // --- 1. types ---------------------------------------------------------

  /// Abstract clock value (model "seconds" -- see the scaled constants).
  type TimeOpt = | NoTime | At(int)

  /// The ten retry counters as the fold computes them. Used three times:
  /// `live` (the fold-derived cached view the dispatch-time readers
  /// consume), `pg.ledger` (the fold over the committed ledger rows) and
  /// `spec` (the reference-fold ghost the tripwire invariants compare
  /// against).
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
  /// PoisonReason). The as-built model's live-only RTimeoutBudget is
  /// gone: post-collapse the timeout cap ends terminal Cancelled on both
  /// observation channels (D1), so no site can produce a timeout poison.
  type Reason =
    | RThreshold
    | RFleetExhausted
    | RTransientBudget
    | RInfraBudget
    | RExemptInfraBudget
    | RPermanent

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

  /// What poisoned the building derivation: a failure-event verdict (the
  /// fold), or the dispatch-time fleet-exhaust backstop (no fold event).
  /// The as-built RecoveryPoison source is gone: recovery rebuilds the
  /// view from the same fold the live path uses, and a committed
  /// at-budget history always has its verdict persisted with it (the
  /// appending transaction is atomic), so recovery never originates a
  /// poison the fold did not.
  type PoisonSrc = | NoPoison | EventPoison | DispatchPoison

  /// The durable view: the derivations row's status half (still owned by
  /// the status persist inside the appending transaction) plus the
  /// attempt ledger, carried as its fold (`ledger`) -- the fold is
  /// incremental, so carrying its state is equivalent to folding the
  /// committed rows at every state. The four per-counter mirror columns
  /// of the as-built model are retired (their writers were deleted at
  /// the Phase-1b cutover).
  type PgStatus = | PgNonTerminal | PgPoisoned | PgCancelled | PgCompleted
  type PgRow = {
    status: PgStatus,
    poisonedAt: TimeOpt,
    ledger: Counters,
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
  /// effective delivery is all the correlation can consume).
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
  /// the executor field where the fold ignores it. FEstablishedCrash is
  /// the Phase-1b addition: the correlation-TTL sweep's establishment of
  /// a released attempt whose classifying report never arrived (C2).
  type FoldEvent =
    | FTransient({ at: int, exec: str })
    | FInfra({ at: int, exempt: bool, atCap: bool })
    | FPermanent({ at: int, exec: str })
    | FWorkerTimeout(int)
    | FDisconnect(int)
    | FControllerTermination({ at: int, promoted: bool, atCap: bool })
    | FControllerDeadlineExceeded(int)
    | FBackstopTimeout({ at: int, exec: str })
    | FEstablishedCrash({ at: int, exec: str })
    | FDispatched(int)
    | FResubmitReset(int)
    | FCacheHitClear(int)
    | FPoisonCleared(int)

  /// Which entry point (or reset path) the most recent accounting
  /// observation went through, plus per-observation facts the
  /// per-event invariants and witnesses read. Overwritten by every
  /// accounting action; ONone between them. OEstablish is the
  /// establishment sweep's observation (Phase 1b).
  type ObsKind =
    | ONone | OE1 | OE2 | OE3 | OE4 | OE5 | OE6 | OE7 | OE8 | OEstablish
    | OE9Dispatch | OResubmit | OCacheHit | OPoisonClear | OTtlExpire | OFailover
  type LastObs = {
    kind: ObsKind,
    chargedAny: bool,       // some budget counter was charged by this event
    doubleCharge: bool,     // the same budget was charged twice by this event
    windowReset: bool,      // the I-127 stale-window reset fired
    exemptFallthrough: bool,// ...and it fired on an exempt under-cap event
    promoted: bool,         // the event's floor outcome
    atCap: bool,
    lateInstallment: bool,  // controller report correlated through an rd entry
    raceAhead: bool,        // controller report used the still-live executor
    terminal: bool,         // the event produced a terminal state
    dropped: bool,          // the status guard dropped the observation (already terminal)
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
  /// attemptsBoundedGlobal). Post-collapse this is a HOLD bound in the
  /// crash regime: budget-justified charges plus the bounded
  /// un-established in-flight window, strictly below MAX_ATTEMPTS so the
  /// invariant is not satisfied by the dispatch ceiling alone.
  const ATTEMPT_BOUND: int
  /// Leader failovers allowed (0 outside the failover regime).
  const MAX_FAILOVERS: int
  /// Appending transactions that may fail (0 outside the fault regime).
  /// Replaces the as-built model's lost-mirror-write budget: post-066
  /// the charge, the verdict and the status persist commit or fail as
  /// one transaction.
  const TX_FAULTS: int
  /// Channel/alphabet switches (one per regime concern).
  const ENABLE_WORKER: bool     // worker-reported failure classes (E1-E4)
  const ENABLE_PODDEATH: bool   // pod deaths + controller channel (E5-E7)
  const ENABLE_CRASH: bool      // the no-report crash death kind + establishment (C2)
  const ENABLE_WEDGE: bool      // the wedge end + backstop (E8)
  const ENABLE_RESETS: bool     // resubmit / cache-hit / TTL-admin clear
  /// The materialization attempt class (substitution-replacement Phase A,
  /// design §2.5/§9.2): false = the regime explores the build-only
  /// alphabet (bit-identical to the pre-Phase-A state space; the three
  /// materialization counters and the snapshot ghost stay frozen at
  /// their init values); true = store-replica materialization attempts
  /// and the establishment crash channel interleave with the build
  /// alphabet (the kind-partition invariants' regime).
  const ENABLE_MATERIALIZATION: bool

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
    status: PgNonTerminal, poisonedAt: NoTime, ledger: zeroCounters,
  }

  pure val freshExec: ExecRec = {
    registered: true, draining: false, runningOurs: false,
    lastCompletedOurs: false, rd: NoTime, ctrl: NoCtrl, ctrlStale: false,
    rdForCtrl: false, deathCharges: 0,
  }

  pure val noObs: LastObs = {
    kind: ONone, chargedAny: false, doubleCharge: false, windowReset: false,
    exemptFallthrough: false, promoted: false, atCap: false,
    lateInstallment: false, raceAhead: false, terminal: false, dropped: false,
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

  /// exhausts_fleet (retry_policy.rs / placeable()): every statically
  /// eligible, registered, non-draining worker is in the exclusion set;
  /// an empty exclusion set or an empty eligible fleet never exhausts
  /// (sched.dispatch.fleet-exhaust+4's empty-fleet defer).
  pure def exhaustsFleet(failed: Set[str], eligible: Set[str]): bool = and {
    failed != Set(),
    eligible != Set(),
    eligible.forall(w => failed.contains(w)),
  }

  /// RetryState::clear() / the cache-hit ledger reset -- nine of the ten
  /// fields; backoff_until survives.
  pure def clearForCacheHit(c: Counters): Counters =
    { ...zeroCounters, backoffUntil: c.backoffUntil }

  // -- The reference fold (retry_policy.rs::apply), transcribed ---------
  //
  // Post-collapse this is no longer only the specification ghost: it IS
  // the decision arithmetic every entry point executes (decide() inside
  // the appending transaction). The actions below advance `live`,
  // `pg.ledger` and the `spec` ghost with the same application of this
  // function; keep it in lockstep with rio-scheduler/src/retry_policy.rs.

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

      // E6: handle_executor_termination. Post-collapse (T-1b.9): the
      // at-cap increment stamps the window anchor (D2) and the promoted
      // arm charges the exemption budget and poisons at its cap (D3).
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

      // E7: handle_deadline_exceeded. Post-collapse (T-1b.10): terminal
      // Cancel at the cap (D1), the same verdict as the worker channel.
      | FControllerDeadlineExceeded(at) =>
        if (c0.timeoutCount >= MAX_TIMEOUT)
          { c: c0, v: VCancel }
        else
          { c: { ...c0, timeoutCount: c0.timeoutCount + 1 }, v: VRequeue }

      // E8: tick_process_backstop_timeouts -- record + threshold check,
      // decided and persisted at the charging site (T-1b.6).
      | FBackstopTimeout(e) =>
        val c1 = { ...c0,
          failedBuilders: c0.failedBuilders.union(Set(e.exec)),
          failureCount: c0.failureCount + 1 }
        if (thresholdReached(c1))
          { c: { ...c1, poisonedAt: At(e.at) }, v: VPoison(RThreshold) }
        else
          { c: c1, v: VRequeue }

      // The establishment sweep (T-1b.11, C2): an unreported executor
      // crash, once established, charges the threshold/exclusion budget
      // -- failed_builders[executor] + failure_count, nothing else
      // (decision P1) -- and re-checks the threshold.
      | FEstablishedCrash(e) =>
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

  /// Building derivation: scheduler-visible status, the fold-derived
  /// cached counter view (RetryState as the dispatch-time readers
  /// consume it), and how it was poisoned (if it was).
  var dStatus: DStatus
  var live: Counters
  var poisonSrc: PoisonSrc

  /// The reference-fold ghost: the counters and verdict the fold
  /// computes over the observed accounting-event history. Post-collapse
  /// the entry points advance their own state with the same fold, so
  /// live == spec is a tripwire, not a discovery instrument.
  var spec: Counters
  var specVerdict: Verdict

  /// The durable view: the derivations row's status half plus the
  /// attempt-ledger fold, advanced atomically with `live` by every
  /// committed appending transaction.
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
  /// against it (any poison cycle) -- the truth the durable ledger is
  /// bounded by.
  var everFailedBuilders: Set[str]
  /// Failovers / appending-transaction-failure budgets remaining.
  var failoversLeft: int
  var txFaultsLeft: int
  /// GHOST: the most recent accounting observation (per-event invariants
  /// and witnesses read this; overwritten each time).
  var last: LastObs
  /// GHOST: poison-reason tag of the most recent poison (witnesses).
  var lastPoisonReason: Reason
  /// GHOST: true exactly in the state produced by the failover action
  /// (cleared by every subsequent action); scopes the recovery
  /// invariants to the post-failover state.
  var justFailedOver: bool

  /// Bughunt wave A2 (bug_279): the TRAILING run of build-lane
  /// worker-abort free-close rows — admit_worker_abort's counted
  /// prefix (rio-retry-kernel/src/lib.rs: Attempt + Disconnected +
  /// Worker, build lane only). Every other build-lane row append
  /// resets it (the kernel's run break); materialization actions skip
  /// it (matLeavesBuildUntouched — the lane discipline). The uncharged
  /// worker-abort close (pullWorkerAbort) is admitted only strictly
  /// below the bound, so the run never exceeds it.
  var freeCloseRun: int

  // --- 5. derived state ---------------------------------------------------

  /// Mirror of rio-retry-kernel's WORKER_ABORT_FREE_CLOSES (lib.rs);
  /// keep in lockstep with the kernel constant.
  pure val WORKER_ABORT_FREE_CLOSES: int = 3

  /// The eligible fleet as the fleet-exhaust check sees it: registered,
  /// non-draining slots (static eligibility -- kind/system/features --
  /// is uniform here).
  def eligibleFleet: Set[str] =
    EXECUTORS.filter(w => execs.get(w).registered and not(execs.get(w).draining))

  /// The slot's previous death is still awaiting establishment: a
  /// recently_disconnected entry exists and no classifying controller
  /// report for THAT death is deliverable. Until the establishment (or a
  /// classifying report) resolves it, the slot is not offered this
  /// derivation again -- the identity-freshness abstraction documented in
  /// the header (production replacement pods are fresh identities; the
  /// dead identity never re-registers).
  def establishmentPending(w: str): bool = and {
    execs.get(w).rd != NoTime,
    or { execs.get(w).ctrl == NoCtrl, not(execs.get(w).rdForCtrl) },
  }

  /// Placement candidates for dispatch: eligible, not excluded by
  /// hard_filter's fold-derived failed_builders check, idle, and not
  /// awaiting establishment of a previous death.
  def placeable: Set[str] =
    eligibleFleet.filter(w => and {
      not(live.failedBuilders.contains(w)),
      not(execs.get(w).runningOurs),
      not(establishmentPending(w)),
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
    txFaultsLeft >= 0 and txFaultsLeft <= TX_FAULTS,
    live.count >= 0 and live.count <= MAX_RETRIES,
    live.infraCount >= 0 and live.infraCount <= MAX_INFRA,
    live.timeoutCount >= 0 and live.timeoutCount <= MAX_TIMEOUT,
    live.exemptInfraCount >= 0 and live.exemptInfraCount <= MAX_EXEMPT,
    live.failedBuilders.subseteq(EXECUTORS),
    pg.ledger.failedBuilders.subseteq(EXECUTORS),
    // the in-flight attempt is consistent with the scheduler view (the
    // converse does not hold: a late controller report or establishment
    // can poison the derivation while its next attempt is still in
    // flight -- the zombie execution whose eventual completion the
    // status guard drops)
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
    // The materialization attempt class (substitution-replacement
    // Phase A): the bounded-counter ceilings. The finiteness of the
    // extended state space is itself a checked invariant, not an
    // assumption (the SC-1 no-unbounded-variables rule); a build-side
    // action edited to bump a materialization counter has no ceiling
    // guard and overruns these bounds within one regime's attempt
    // budget (the build->materialization tripwire direction).
    matInfraCount >= 0 and matInfraCount <= MAX_MATERIALIZATION_ATTEMPTS,
    matSchedulerInfraCount >= 0 and matSchedulerInfraCount <= matInfraCount,
    matUnobtainableCount >= 0 and matUnobtainableCount <= MAX_MAT_UNOBTAINABLE,
  }

  /// The model-checked form of sched.retry.counters-refine-history+2:
  /// the cached counter view equals the reference fold of the observed
  /// history (carried incrementally as `spec`). poisoned_at is excluded
  /// when the poison came from a site outside the fold's event alphabet
  /// (the dispatch-time fleet exhaust). Post-collapse the entry points
  /// compute their own state with the fold, so this is true by
  /// construction -- kept as a cheap regression tripwire against the two
  /// halves drifting apart in a future encoding change.
  val countersRefineHistory =
    if (poisonSrc == DispatchPoison)
      { ...live, poisonedAt: NoTime } == { ...spec, poisonedAt: NoTime }
    else
      live == spec

  /// The budget verdict is channel-invariant: the live terminal state
  /// agrees with the fold's verdict over the observed history
  /// (sched.retry.verdict-channel-invariant, scoped to the terminal
  /// budget verdicts). Post-collapse this HOLDS in every regime: the D1
  /// divergence is gone (the controller-observed timeout cap ends
  /// Cancelled, exactly as the fold and the worker channel say).
  val verdictMatchesFold = and {
    (dStatus == DPoisoned and poisonSrc == EventPoison) implies
      match specVerdict { | VPoison(r) => true | VRequeue => false | VCancel => false },
    dStatus == DCancelled implies specVerdict == VCancel,
  }

  /// sched.retry.attempts-bounded+2, the per-event clauses: a single
  /// accounting observation never charges the same budget twice, and the
  /// charge-or-terminal discipline is total over the post-collapse event
  /// alphabet (the Stage-B clause-coverage audit's narrowing is closed):
  /// every worker-reported failure or backstop observation charges or is
  /// terminal; every controller-report or establishment observation
  /// charges, is terminal, or was dropped by the status guard because
  /// the derivation had already left the poison-able statuses; and the
  /// bare disconnect observation never charges (its charge vehicle is
  /// the establishment -- sched.retry.per-executor-budget+4's
  /// established-crash clause -- so an uncharged disconnect is the spec'd
  /// behaviour, not a leak).
  val attemptsChargedOnce = and {
    not(last.doubleCharge),
    (last.kind == OE1 or last.kind == OE2 or last.kind == OE3 or
     last.kind == OE4 or last.kind == OE8)
      implies (last.chargedAny or last.terminal),
    (last.kind == OE6 or last.kind == OE7 or last.kind == OEstablish)
      implies (last.chargedAny or last.terminal or last.dropped),
    (last.kind == OE5) implies not(last.chargedAny),
  }

  /// sched.retry.attempts-bounded+2, the boundedness clause: the number
  /// of physical attempts is bounded by what the enabled budgets
  /// justify (ATTEMPT_BOUND is a regime constant). Post-collapse this
  /// HOLDS in the crash regime: every crash is established and charged
  /// (C2), so the loop is bounded by the threshold budget plus the
  /// bounded un-established in-flight window.
  val attemptsBoundedGlobal = physAttempts <= ATTEMPT_BOUND

  /// The model-checked form of sched.retry.no-double-count: one
  /// physical executor death produces at most one counted accounting
  /// event for the derivation, over every observation subset and order
  /// the channels can produce (including the establishment).
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

  /// No durable or recovered counter exceeds what the history supports:
  /// every executor the ledger names as a failed builder really did have
  /// a charging failure recorded against it at some point, and the
  /// post-failover view comes from the ledger alone.
  val recoveryNeverFabricatesFailures = and {
    pg.ledger.failedBuilders.subseteq(everFailedBuilders),
    justFailedOver implies live.failedBuilders.subseteq(pg.ledger.failedBuilders),
  }

  /// The durable view contains every charge the cached view carries: an
  /// executor recorded into the live failed_builders set is also in the
  /// ledger fold. Post-collapse this HOLDS everywhere -- the appending
  /// transaction is the only writer of both sides (the as-built D4
  /// falsification, E8's never-mirrored backstop charge, is gone).
  val durableMirrorsCharges = live.failedBuilders.subseteq(pg.ledger.failedBuilders)

  /// The fleet-exhaust poison is sound: it only fires when the exclusion
  /// set covers a non-empty eligible fleet (the empty fleet defers --
  /// sched.dispatch.fleet-exhaust+4).
  val placementSound =
    (dStatus == DPoisoned and poisonSrc == DispatchPoison) implies
      lastPoisonReason == RFleetExhausted

  /// A poison clear that took effect in memory has taken effect durably:
  /// the admin/TTL clear is PG-first (a failed PG clear aborts the whole
  /// clear -- sched.admin.clear-poison's both-stores obligation), so in
  /// the state right after a clear observation the durable row is never
  /// still poisoned. Deliberately scoped to the TTL/admin path: the
  /// cache-hit clear's PG half rides the same appending transaction as
  /// its reset row, but the pre-collapse best-effort scoping is kept so
  /// the invariant's meaning is unchanged across the re-encode.
  val clearedPoisonClearsDurably =
    (last.kind == OTtlExpire or last.kind == OPoisonClear) implies
      pg.status != PgPoisoned

  /// The TTL/admin clear scrubs the durable exclusion set too: the
  /// poison_cleared ledger row resets the fold, so in the state right
  /// after a clear observation the durable view carries no
  /// failed-builder exclusions for the next cycle to inherit at
  /// recovery. Same antecedent scoping as clearedPoisonClearsDurably.
  val clearedPoisonScrubsExclusions =
    (last.kind == OTtlExpire or last.kind == OPoisonClear) implies
      pg.ledger.failedBuilders == Set()

  /// Failover preserves the poison set: a durable row that still says
  /// Poisoned (and is not TTL-expired) is reloaded as Poisoned -- the
  /// "poison set preserved exactly minus TTL-expired entries" clause of
  /// sched.retry.recovery-projection+3. DCompleted is excluded from the
  /// antecedent: a cache-hit clear can complete the node while the
  /// durable row update rides a later reconcile, and the gone-arm
  /// preserves the completion.
  val recoveryPreservesPoisonStatus =
    justFailedOver implies (
      and {
        pg.status == PgPoisoned,
        match pg.poisonedAt { | NoTime => true | At(t) => now - t <= POISON_TTL },
        dStatus != DCompleted,
      } implies dStatus == DPoisoned
    )

  /// The Phase-1 acceptance property (sched.retry.failover-budget;
  /// design-§3 FailoverPreservesHistory): the post-failover decision
  /// inputs are never more permissive than the pre-failover ones. In
  /// this encoding the recovered view IS the durable ledger fold and the
  /// appending-transaction discipline keeps the pre-failover cached view
  /// equal to that same fold, so the budget counters must come back
  /// exactly -- per counter, not merely no-smaller: any forgiveness
  /// re-introduced into the failover arm, or any charge that stops being
  /// durable at its observation, falsifies this. backoff_until is
  /// excluded (pacing, not a budget: the dispatch-time clear has no
  /// ledger row, so the recovered deadline may be stricter than the
  /// pre-failover RAM one); poisoned_at is owned by
  /// recoveryPreservesPoisonStatus and the dispatch-poison carve-out;
  /// terminal/absent rows are not reloaded and carry no decision inputs.
  val failoverPreservesHistory =
    (justFailedOver and (dStatus == DReady or dStatus == DPoisoned)) implies and {
      live.count == pg.ledger.count,
      live.resubmitCycles == pg.ledger.resubmitCycles,
      live.infraCount == pg.ledger.infraCount,
      live.timeoutCount == pg.ledger.timeoutCount,
      live.exemptInfraCount == pg.ledger.exemptInfraCount,
      live.failureCount == pg.ledger.failureCount,
      live.failedBuilders == pg.ledger.failedBuilders,
      live.lastInfraAt == pg.ledger.lastInfraAt,
    }

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
  /// Violated when the controller-observed timeout cap ends terminal
  /// Cancelled (the post-collapse D1 shape; the as-built model's
  /// controller-cap POISON witness has no producer any more).
  val noControllerCapCancelled = not(dStatus == DCancelled and last.kind == OE7)
  /// Violated when the I-127 stale-window reset fires.
  val noWindowReset = not(last.windowReset)
  /// Violated when the window reset fires on an exempt under-cap event
  /// (the as-built E2 fall-through the fold reproduces).
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
  /// Violated when an unreported executor crash is established and
  /// charges the threshold/exclusion budget (the C2 charge is reachable).
  val noEstablishedCrashCharge = not(last.kind == OEstablish and last.chargedAny)
  /// Violated when the crash-only alphabet reaches a failure-terminal
  /// state (the no-report crash loop now terminates).
  val noCrashLoopTerminal = not(failureTerminal)
  /// Violated when a poison TTL expiry clears the derivation.
  val noTtlExpiry = not(last.kind == OTtlExpire)
  /// Violated when a resubmit reset starts a second poison cycle.
  val noResubmitReset = not(live.resubmitCycles > 0)
  /// Violated when a cache hit clears a poisoned derivation.
  val noCacheHitClear = not(last.kind == OCacheHit)
  /// Violated when the dependent is cascaded.
  val noCascade = not(depStatus == DepFailed)

  /// Bughunt wave A2 (bug_279): the trailing run of uncharged
  /// worker-abort closes never exceeds the kernel bound — a worker
  /// looping pull -> Cancelled cannot mint unlimited uncharged
  /// requeues; the bound-th consecutive abort can only take the
  /// charged fall-through (the existing report classes). Maintained by
  /// pullWorkerAbort's admission guard, exactly as admit_worker_abort
  /// gates the production intake.
  val boundedFreeRequeues = freeCloseRun <= WORKER_ABORT_FREE_CLOSES
  /// Violated when the free-close run actually reaches the bound (the
  /// admission gate's contended edge is reachable — non-vacuity for
  /// boundedFreeRequeues).
  val canReachFreeCloseBound = not(freeCloseRun == WORKER_ABORT_FREE_CLOSES)
  /// Violated when a leader failover happens with a non-empty,
  /// under-budget recovered history (the failover-budget contention is
  /// reachable).
  val noFailoverWithHistory = not(and {
    justFailedOver,
    or {
      pg.ledger.failedBuilders != Set(),
      pg.ledger.count > 0,
      pg.ledger.infraCount > 0,
      pg.ledger.timeoutCount > 0,
    },
    dStatus == DReady,
  })
  /// Violated when a leader failover lands on a still-poisoned, unexpired
  /// durable row (the recoveryPreservesPoisonStatus contention is
  /// reachable). Added when the Stage-C corpus -- whose G8 calibration
  /// check previously pinned this reachability -- was retired in Phase 2.
  val noFailoverOnPoisonedRow = not(and {
    justFailedOver,
    pg.status == PgPoisoned,
    match pg.poisonedAt { | NoTime => false | At(t) => now - t <= POISON_TTL },
  })

  /// Violated when an appending transaction fails (the bounded
  /// re-delivery fault is reachable in the fault regime).
  val noAttemptTxFailure = not(txFaultsLeft < TX_FAULTS)
  /// Violated when a controller report charges (or poisons) for a death
  /// whose disconnect was already processed -- both channels of one
  /// death observed, the second one counted (the dedup's contended shape).
  val noDualObservation = not(last.lateInstallment and (last.chargedAny or last.terminal))

  // --- 7. actions ---------------------------------------------------------
  // Every action's precondition is the corresponding code path's guard
  // and its effect is that path's state mutation; the entry point it
  // mirrors is named in its doc comment. Every accounting action's
  // counter/verdict arithmetic is one `specApply` application -- the
  // appending transaction's decide() -- assigned to `live`, `pg.ledger`
  // and the `spec` ghost together. Actions are guarded (disabled when
  // their preconditions fail), not always-enabled no-ops.

  /// Advance the abstract clock. (The establishment is modeled as the
  /// pull regime's explicit `pullEstablish` action, never as a silent
  /// timer side effect of the clock; attempts are resolved by a
  /// classifying report or by establishment, never silently dropped.)
  action tick = all {
    freeCloseRun' = freeCloseRun,
    now < MAX_TIME,
    now' = now + 1,
    execs' = execs,
    dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, last' = last,
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
    freeCloseRun' = 0,
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
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    last' = last, lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The running build's pod dies without a worker report: the stream
  /// will close (disconnect pending) and the controller learns a
  /// termination reason it will report (OOM / DeadlineExceeded), or the
  /// death is a non-resource crash the controller has nothing useful to
  /// say about (the C2 shape -- its charge arrives at establishment).
  action buildEndsPodDeath(w: str, kind: DeathKind): bool = all {
    freeCloseRun' = 0,
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
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    last' = last, lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7c. the shared infra-classification arm ----------------------------

  /// E2's body, shared by the three worker-reported infra classes: one
  /// specApply (the exempt block with its own cap, the I-127 window
  /// reset and its exempt fall-through, the non-exempt cap check and
  /// charge with the anchor stamp), acted on per the verdict. The
  /// windowReset/exemptFallthrough ghost flags are recomputed locally
  /// for the witnesses; they do not feed the counters.
  // Mirrors: completion.rs::handle_infrastructure_failure through
  //          append_and_decide_in_tx + classify().
  action infraArm(w: str, execAfter: ExecRec, fleetAfter: Set[str], exempt: bool, atCap: bool, consumesPromotion: bool): bool = all {
    val applied = specApply(spec, FInfra({ at: now, exempt: exempt, atCap: atCap }), fleetAfter)
    val exemptCapHit = exempt and (live.exemptInfraCount + 1 >= MAX_EXEMPT)
    val windowElapsed = and {
      not(atCap),
      not(exemptCapHit),
      match live.lastInfraAt { | NoTime => false | At(t) => now - t > INFRA_WINDOW },
    }
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
      live' = applied.c,
      execs' = execs.put(w, execAfter),
      attempt' = NoAttempt,
      promotionsLeft' = if (consumesPromotion) promotionsLeft - 1 else promotionsLeft,
      match applied.v {
        | VPoison(r) => all {
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          lastPoisonReason' = r,
          pg' = { status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OE2, chargedAny: true, terminal: true,
            windowReset: windowElapsed, promoted: consumesPromotion, atCap: atCap },
        }
        | VRequeue => all {
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE2, chargedAny: true, windowReset: windowElapsed,
            exemptFallthrough: windowElapsed and exempt,
            promoted: consumesPromotion, atCap: atCap },
        }
        | VCancel => all {
          // unreachable: the infra arm never cancels
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE2, chargedAny: true, windowReset: windowElapsed,
            exemptFallthrough: windowElapsed and exempt,
            promoted: consumesPromotion, atCap: atCap },
        }
      },
    },
  }

  // --- 7e. the lost controller report -------------------------------------

  /// The controller's report never lands (controller down, JOB_TTL
  /// expired): the death stays unclassified. If the released attempt's
  /// entry is still pending, the establishment (the TTL sweep) becomes
  /// its classifier and charge vehicle.
  action ctrlReportLost(w: str): bool = all {
    freeCloseRun' = freeCloseRun,
    execs.get(w).ctrl != NoCtrl,
    execs' = execs.put(w, { ...execs.get(w), ctrl: NoCtrl }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7g. the appending-transaction fault --------------------------------

  /// The appending transaction for a pending worker CompletionReport
  /// fails (PG blip): nothing is inserted, nothing is charged, no status
  /// is persisted, and the completion event is re-enqueued to the
  /// actor's own mailbox for bounded re-delivery -- the derivation stays
  /// in its pre-report state until the record lands (the design's
  /// deliberate inversion of the old best-effort posture). The pending
  /// report stays in flight here and a later report step (the pull
  /// regime's pullReportOutcome) is the re-delivery. Replaces the as-built model's independently-lost
  /// per-counter mirror writes: post-066 the charge, the verdict and the
  /// status persist commit or fail as one unit.
  // Mirrors: completion.rs append_and_decide_in_tx error arm +
  //          requeue_failure_completion.
  action attemptTxFails(w: str): bool = all {
    freeCloseRun' = freeCloseRun,
    txFaultsLeft > 0,
    match attempt { | Reported(r) => r.exec == w | _ => false },
    dStatus == DRunning,
    txFaultsLeft' = txFaultsLeft - 1,
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    last' = last, lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  // --- 7i. resets ----------------------------------------------------------

  /// The dag::merge resubmit reset: a retriable terminal node
  /// (Cancelled unconditionally; Poisoned iff resubmit_cycles <
  /// POISON_RESUBMIT_RETRY_LIMIT) gets a fresh per-cycle state with
  /// resubmit_cycles incremented. The reset is itself a durable ledger
  /// row (the suffix cut point), appended in the same transaction as
  /// the merge persist; the dependent's DependencyFailed is reset by the
  /// same merge.
  // Mirrors: dag/mod.rs (the resubmit-reset arm, reset_row_for) +
  //          merge.rs + derivation.rs::is_retriable_on_resubmit.
  action resubmitReset = all {
    freeCloseRun' = 0,
    ENABLE_RESETS,
    or {
      dStatus == DCancelled,
      and { dStatus == DPoisoned, live.resubmitCycles < RESUBMIT_LIMIT },
    },
    val applied = specApply(spec, FResubmitReset(now), eligibleFleet)
    all {
      dStatus' = DReady,
      poisonSrc' = NoPoison,
      live' = applied.c,
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = { status: PgNonTerminal, poisonedAt: NoTime, ledger: applied.c },
      depStatus' = DepReady,
      last' = { ...noObs, kind: OResubmit },
    },
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The cache-hit clear: the poisoned derivation's output turned up in
  /// the store (another tenant built it / a re-probe found it
  /// substitutable), so the failure history is moot: a cache_hit_clear
  /// reset row (nine of ten counters cleared, backoff_until survives) is
  /// appended in the same transaction as the site's persist and the
  /// derivation transitions to Completed.
  // Mirrors: merge.rs::apply_cached_hits / dispatch.rs spawn_substitute
  //          reprobe lane (the Poisoned arm).
  action cacheHitClear = all {
    freeCloseRun' = 0,
    ENABLE_RESETS,
    dStatus == DPoisoned,
    val applied = specApply(spec, FCacheHitClear(now), eligibleFleet)
    all {
      dStatus' = DCompleted,
      poisonSrc' = NoPoison,
      live' = applied.c,
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = { status: PgCompleted, poisonedAt: NoTime, ledger: applied.c },
      last' = { ...noObs, kind: OCacheHit },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The poison TTL expiry (the admin ClearPoison path is identical
  /// minus the time gate and is not modeled separately): PG is cleared
  /// FIRST (a failed clear aborts -- equivalently, the action does not
  /// fire) with the poison_cleared ledger row, then the node is removed
  /// from the DAG; the next merge would re-insert it fresh (not modeled
  /// -- DAbsent is a sink).
  // Mirrors: housekeeping.rs::tick_process_expired_poisons +
  //          db::clear_poison (the full reset).
  action poisonTtlExpires = all {
    freeCloseRun' = freeCloseRun,
    ENABLE_RESETS,
    dStatus == DPoisoned,
    poisonTtlElapsed,
    val applied = specApply(spec, FPoisonCleared(now), eligibleFleet)
    all {
      dStatus' = DAbsent,
      poisonSrc' = NoPoison,
      live' = applied.c,
      spec' = applied.c,
      specVerdict' = applied.v,
      pg' = freshPg,
      last' = { ...noObs, kind: OTtlExpire },
    },
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
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
    txFaultsLeft' = TX_FAULTS,
    last' = noObs,
    lastPoisonReason' = RThreshold,
    justFailedOver' = false,
    everFailedBuilders' = Set(),
    freeCloseRun' = 0,
    // The materialization attempt class (substitution-replacement Phase A).
    matInfraCount' = 0,
    matSchedulerInfraCount' = 0,
    matUnobtainableCount' = 0,
    matObservedBuild' = NoBuildSnap,
  }

  // --- 9. shared step wrappers (composed by pullStep, section 12) ----------
  // Every disjunct is a named action with its nondet choices bound
  // directly inside it.

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
  action ctrlReportLostAny = { nondet w = EXECUTORS.oneOf() ctrlReportLost(w) }
  action attemptTxFailsAny = { nondet w = EXECUTORS.oneOf() attemptTxFails(w) }

// ===========================================================================
// --- 10. THE PULL-MODE ENVIRONMENT (executor-lifecycle campaign, slice 1b —
// the T-0e.3 re-derivation executed). Same fold, same invariants, different
// event-arrival environment: the events the fold consumes arrive through the
// pull-path unaries (`PullAssignment` opens the attempt, `ReportOutcome`
// carries the worker classes, `ReportAttemptOutcome` is the controller's
// idempotent pod-terminal fill) and the establishment sweep, instead of the
// dispatch stream / disconnect / recently_disconnected correlation channels.
// The as-built channel regimes above stay authoritative for the stream path
// until the executor campaign's 1c' retires it; this regime is additive.
//
// (Substitution-replacement Phase A restructuring, T-5.1: these
// definitions moved verbatim from the former standalone retryPolicyPull
// module into the core so that every regime module — the build-only
// retryPolicyPull and the materialization-coexistence retryPolicyPullMat,
// both at the end of this file — can instantiate them with its own
// constants. The action text is unchanged; only the module boundary and
// the constant binding moved. The pull-mode regime's documentation below
// is unchanged.)
//
// WHAT CHANGES (exactly the T-0e.3 list in
// docs/spec/models/executor-invariant-map.md):
//   - dispatchTo -> pullOpensAttempt: the attempt opens at PullAssignment
//     (Ready->Running + exec mint + row insert in one fenced transaction).
//     The dispatch-time eligibility/exclusion precondition moves to the
//     spawn-intent gate per AD2: the EXECUTORS constant is the set of
//     SOURCE NODES (not pod slots), the exclusion set carries node keys,
//     and a pull can only arrive from a non-excluded spawnable source
//     (the controller renders anti-affinity from the intent's
//     excluded_nodes, so an excluded source never hosts the pod).
//   - E6/E7 (controller channel + recently_disconnected correlation) ->
//     pullAttemptOutcomeOom / pullAttemptOutcomeDeadline: the single
//     idempotent column fill keyed by attempt identity. The fold events
//     and their charging semantics are UNCHANGED (FControllerTermination /
//     FControllerDeadlineExceeded); what changes is the dedup vehicle —
//     the `WHERE termination_reason IS NULL` fill discipline replaces the
//     correlation entry. A fill that finds no open attempt (never pulled,
//     already closed by the worker report or the establishment, or a
//     duplicate delivery) is the no-attempt no-op: pullAttemptOutcomeNoOp
//     consumes the pending report, charges nothing, creates nothing.
//   - establishUnreportedCrash -> pullEstablish: the precondition becomes
//     "open attempt past deadline + report-slack with no terminal row"
//     instead of "released attempt with no deliverable classifying
//     report". The charge (FEstablishedCrash: failed_builders[source] +
//     failure_count + threshold re-check) is unchanged.
//   - E9 / fleet-exhaust re-keyed per AD2: the exclusion set carries
//     source nodes; the exhausted-universe verdict is evaluated at the
//     spawn-intent gate (excluded ⊇ spawnable -> NoEligibleSource ->
//     Poison(FleetExhausted)). Encoded two ways, matching the code: the
//     fold application at every charging site receives the live
//     spawnable-source set (so a charge that completes the exclusion of
//     the whole universe poisons in the same transaction — the
//     small-fleet clause of sched.retry.per-executor-budget+4: the
//     threshold is effectively min(threshold, |sources|)), and the
//     standalone spawnGateExhaust action covers the no-new-charge case
//     where the spawnable universe shrinks under an existing exclusion
//     set (sourceLeavesPool models the shrink; the empty-universe defer
//     arm of sched.dispatch.fleet-exhaust+4 is preserved).
//   - Identity freshness is structural: a fresh pod per attempt. No
//     respawn action, no draining bookkeeping, no executor-map identity
//     reuse; a source node hosts at most one open attempt for this
//     derivation at a time and stays spawnable after the attempt closes
//     (only the exclusion set removes it).
//   - What stays imported unchanged: specApply (the fold), and the
//     invariants attemptsChargedOnce, verdictMatchesFold,
//     failoverPreservesHistory, countersRefineHistory, noDoubleCount,
//     durableMirrorsCharges, poisonIsTerminalUntilCleared,
//     cascadeReachesExactlyTheDependents, recoveryNeverFabricatesFailures,
//     placementSound, clearedPoisonClearsDurably,
//     clearedPoisonScrubsExclusions, recoveryPreservesPoisonStatus,
//     boundsOK — none are redefined here.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - The establishment window's clock (deadline + report-slack): the
//     pullEstablish action is enabled as soon as an open attempt exists,
//     over-approximating production timing exactly as the as-built
//     regimes over-approximate the 60 s correlation TTL. This is
//     conservative for the safety invariants (more interleavings,
//     including the establishment racing the controller fill and the
//     in-flight worker report); the only-after-the-window timing is
//     asserted end-to-end by the pull-canary VM scenario and the
//     establishment-sweep unit battery (sched.attempt.establishment-window).
//   - The NotYetReady pull outcome (OA6) and the idempotent re-pull of an
//     open attempt: both are charge-free reads (no row, no fold event),
//     i.e. stuttering steps at this model's resolution. Their semantics
//     are owned by the scheduler pull battery and the Model-S re-target
//     at 1c'.
//   - The pre-pull pod death (a pod that dies before its first
//     PullAssignment): it never opens an attempt, so the controller's
//     report resolves to no attempt row — the same no-op arm
//     pullAttemptOutcomeNoOp encodes for the already-closed case. The
//     model does not distinguish the two producers of that no-op.
//   - Pull/report RPC retry envelopes and unary transport faults: the
//     unaries are idempotent and bounded-retried by the pod (builder
//     consts); a failed appending transaction behind ReportOutcome is the
//     same atomic-tx inversion the as-built fault regime prices
//     (TX_FAULTS = 0 here).
//   - The wedge as a distinct state (ENABLE_WEDGE = false): a pull-mode
//     build that never reports is closed by activeDeadlineSeconds (the
//     controller's deadline fill) or by the establishment sweep; with
//     pullEstablish enabled on any open attempt the Wedged state adds no
//     distinct outcome, and the as-built backstop (E8) has no pull-mode
//     successor (the establishment sweep is the only scheduler-side
//     time-based repair).
//   - Multi-derivation interference and the spawn/Job lifecycle itself
//     (Model J/N territory); the busy bridge and synthesize-on-delete
//     arms are controller-side and carry no retry charge.
//
// The wired checks, their invariant lists and the witness wiring live in
// nix/quint.nix (the executor-campaign appended block). Measured state
// counts, depths and wall-clocks live in the introducing commit's message
// and the checks' transcripts, never here.
// ===========================================================================

  // --- derived state (pull-mode environment) ------------------------------

  /// The spawnable-source universe as the spawn-intent gate sees it: the
  /// pool's nodes still present in the controller's informer view.
  /// (eligibleFleet's registered/non-draining filter carries exactly
  /// that meaning here: sourceLeavesPool clears `registered`, nothing
  /// ever sets `draining`.)
  def spawnableSources: Set[str] = eligibleFleet

  /// Where the next pod can be placed: spawnable and not excluded by the
  /// fold-derived, source-keyed exclusion set (the AD2 anti-affinity
  /// render of the intent's excluded_nodes).
  def pullPlaceableSources: Set[str] =
    spawnableSources.filter(n => not(live.failedBuilders.contains(n)))

  // --- expect-violation witnesses (pull-specific non-vacuity probes) ------

  /// Violated when a pull has opened an attempt (the post-pull state is
  /// reachable: Ready->Running with an open attempt bound to a source).
  val noPullOpensAttempt = not(dStatus == DRunning and attempt != NoAttempt)

  /// Violated when a controller pod-terminal report arrives with no open
  /// attempt to consume it and is dropped charge-free (the
  /// sched.attempt.no-attempt-no-op arm is reachable).
  val noNoAttemptReportNoop = not(last.kind == OE6 and last.dropped)

  // --- actions (the pull-path environment) ---------------------------------

  /// PullAssignment delivers: the fenced pull transaction opens the
  /// attempt (Ready->Running, exec mint, assignments/executions rows) for
  /// a pod the controller placed on a non-excluded spawnable source.
  /// The backoff defer and the dispatch ceiling gate the pull exactly as
  /// they gated the push (the intent is not deliverable while the defer
  /// is pending), and the fold observes the same FDispatched event
  /// (backoff cleared in RAM, no ledger row).
  // Mirrors: actor/pull.rs::admit_pull (Deliver arm) + the pull
  //          transaction; the AD2 spawn-gate exclusion stands in for
  //          hard_filter.
  action pullOpensAttempt(n: str): bool = all {
    freeCloseRun' = freeCloseRun,
    dStatus == DReady,
    backoffElapsed,
    physAttempts < MAX_ATTEMPTS,
    attempt == NoAttempt,
    pullPlaceableSources.contains(n),
    dStatus' = DRunning,
    attempt' = Running(n),
    execs' = execs.put(n, { ...execs.get(n), runningOurs: true }),
    live' = { ...live, backoffUntil: NoTime },
    val applied = specApply(spec, FDispatched(now), eligibleFleet)
    all {
      spec' = applied.c,
      specVerdict' = applied.v,
    },
    pg' = pg,
    physAttempts' = physAttempts + 1,
    now' = now, poisonSrc' = poisonSrc,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, last' = noObs,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// ReportOutcome intake (E1-E4 over the unary): the worker's
  /// CompletionReport classes charge exactly as on the stream path —
  /// the per-class fold events and verdict handling are unchanged. What
  /// differs is the environment bookkeeping: the pod is gone after its
  /// one report (identity freshness), the source node stays spawnable
  /// (no draining), and the fold's fleet snapshot is the live
  /// spawnable-source set INCLUDING the reporting source — the AD2
  /// spawn-gate evaluation would see that source as both spawnable and
  /// excluded, so a charge that completes the exclusion of the whole
  /// universe poisons FleetExhausted in the same transaction (the
  /// small-fleet clause; on larger universes the distinct-source
  /// threshold fires first, exactly as for the as-built regimes).
  // Mirrors: grpc executor_service.rs::report_outcome ->
  //          completion.rs handle_* per class through
  //          append_and_decide_in_tx (charging semantics unchanged).
  action pullReportOutcome(n: str, class: ReportClass, dual: bool, promoted: bool, atCap: bool): bool = all {
    freeCloseRun' = 0,
    attempt == Reported({ exec: n, class: class, dual: dual }),
    // the floor oracle, exactly as for the stream-path intake
    promoted implies (class == RcInfraOom and promotionsLeft > 0),
    atCap implies class == RcInfraOom,
    not(promoted and atCap),
    (class == RcInfraOom) implies (promoted or atCap),
    dStatus == DRunning,

    // the pod exits after reporting; the source stays in the universe
    val execAfter = { ...execs.get(n), runningOurs: false }
    val sourceFleet = eligibleFleet
    match class {
      // ── E1: transient ──────────────────────────────────────────────
      | RcTransient =>
        val applied = specApply(spec, FTransient({ at: now, exec: n }), sourceFleet)
        all {
          execs' = execs.put(n, execAfter),
          attempt' = NoAttempt,
          spec' = applied.c,
          specVerdict' = applied.v,
          live' = applied.c,
          promotionsLeft' = promotionsLeft,
          match applied.v {
            | VPoison(r) => all {
              dStatus' = DPoisoned,
              poisonSrc' = EventPoison,
              lastPoisonReason' = r,
              pg' = { status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
              depStatus' = DepFailed,
              everFailureTerminal' = true,
              last' = { ...noObs, kind: OE1, chargedAny: true, terminal: true },
            }
            | VRequeue => all {
              dStatus' = DReady,
              poisonSrc' = poisonSrc,
              lastPoisonReason' = lastPoisonReason,
              pg' = { ...pg, ledger: applied.c },
              depStatus' = depStatus,
              everFailureTerminal' = everFailureTerminal,
              last' = { ...noObs, kind: OE1, chargedAny: true },
            }
            | VCancel => all {
              // unreachable: the transient arm never cancels
              dStatus' = DReady,
              poisonSrc' = poisonSrc,
              lastPoisonReason' = lastPoisonReason,
              pg' = { ...pg, ledger: applied.c },
              depStatus' = depStatus,
              everFailureTerminal' = everFailureTerminal,
              last' = { ...noObs, kind: OE1, chargedAny: true },
            }
          },
        }

      // ── E2: the three infra classes, via the shared infra arm ───────
      | RcInfraPutPath => infraArm(n, execAfter, sourceFleet, true, false, false)
      | RcInfraOom => infraArm(n, execAfter, sourceFleet, promoted, atCap, promoted)
      | RcInfraOther => infraArm(n, execAfter, sourceFleet, false, false, false)

      // ── E3: permanent ──────────────────────────────────────────────
      | RcPermanent =>
        val applied = specApply(spec, FPermanent({ at: now, exec: n }), sourceFleet)
        all {
          execs' = execs.put(n, execAfter),
          attempt' = NoAttempt,
          spec' = applied.c,
          specVerdict' = applied.v,
          live' = applied.c,
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          lastPoisonReason' = RPermanent,
          pg' = { status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OE3, chargedAny: true, terminal: true },
          promotionsLeft' = promotionsLeft,
        }

      // ── E4: worker-reported timeout ────────────────────────────────
      | RcTimeout =>
        val applied = specApply(spec, FWorkerTimeout(now), sourceFleet)
        all {
          spec' = applied.c,
          specVerdict' = applied.v,
          live' = applied.c,
          attempt' = NoAttempt,
          promotionsLeft' = promotionsLeft,
          // the dual flag marks the deadline kill the controller also
          // observed: this report is that death's one counted event;
          // the controller's later fill finds the row closed (no-op).
          execs' = execs.put(n, if (dual) { ...execAfter, deathCharges: execAfter.deathCharges + 1 } else execAfter),
          match applied.v {
            | VRequeue => all {
              dStatus' = DReady,
              poisonSrc' = poisonSrc,
              lastPoisonReason' = lastPoisonReason,
              pg' = { ...pg, ledger: applied.c },
              depStatus' = depStatus,
              everFailureTerminal' = everFailureTerminal,
              last' = { ...noObs, kind: OE4, chargedAny: true },
            }
            | VCancel => all {
              dStatus' = DCancelled,
              poisonSrc' = poisonSrc,
              lastPoisonReason' = lastPoisonReason,
              pg' = { ...pg, status: PgCancelled, ledger: applied.c },
              depStatus' = DepFailed,
              everFailureTerminal' = true,
              last' = { ...noObs, kind: OE4, terminal: true },
            }
            | VPoison(r) => all {
              // unreachable: the worker-timeout arm never poisons
              dStatus' = DReady,
              poisonSrc' = poisonSrc,
              lastPoisonReason' = lastPoisonReason,
              pg' = { ...pg, ledger: applied.c },
              depStatus' = depStatus,
              everFailureTerminal' = everFailureTerminal,
              last' = { ...noObs, kind: OE4, chargedAny: true },
            }
          },
        }
    },

    txFaultsLeft' = txFaultsLeft,
    everFailedBuilders' = if (class == RcTransient or class == RcPermanent)
      everFailedBuilders.union(Set(n)) else everFailedBuilders,
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    justFailedOver' = false,
  }

  /// ReportAttemptOutcome, OOM/disk-pressure reason: the controller's
  /// pod-terminal classification lands on the open attempt row, keyed
  /// by attempt identity. The fold event and its {promoted, atCap}
  /// floor outcome are exactly the as-built E6's
  /// (FControllerTermination); the fill closes the attempt and the
  /// verdict acts in the same transaction (requeue or poison). No
  /// correlation entry, no race-ahead/late-installment split: the row
  /// fill is the dedup. (Slice note: the 1a/1b code resolves a pulled
  /// but never-worker-reported pod death via the establishment sweep —
  /// also modeled — and reserves the fill for the reason-only second
  /// installment; this action additionally covers the frozen-contract
  /// arrival order in which the unified fill itself classifies the
  /// open attempt, so both orders are explored.)
  // Mirrors: the T-0e.6 ReportAttemptOutcome contract (C4/C5
  //          unification, WHERE termination_reason IS NULL) for
  //          OOMKilled / EvictedDiskPressure.
  action pullAttemptOutcomeOom(n: str, promoted: bool, atCap: bool): bool = all {
    freeCloseRun' = 0,
    match attempt { | PodDead(p) => p.exec == n and p.kind == DkOom | _ => false },
    execs.get(n).ctrl == CtrlOom,
    promoted implies promotionsLeft > 0,
    not(promoted and atCap),
    promoted or atCap,
    val e = execs.get(n)
    val applied = specApply(spec,
      FControllerTermination({ at: now, promoted: promoted, atCap: atCap }), eligibleFleet)
    all {
      execs' = execs.put(n, { ...e, runningOurs: false, ctrl: NoCtrl,
        deathCharges: e.deathCharges + 1 }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      live' = applied.c,
      promotionsLeft' = if (promoted) promotionsLeft - 1 else promotionsLeft,
      match applied.v {
        | VPoison(r) => all {
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          lastPoisonReason' = r,
          pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OE6, chargedAny: true, terminal: true,
            promoted: promoted, atCap: atCap },
        }
        | VRequeue => all {
          // the fill closes the attempt and requeues the derivation in
          // the same transaction (the pull path has no separate
          // disconnect/reassign step)
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE6, chargedAny: true,
            promoted: promoted, atCap: atCap },
        }
        | VCancel => all {
          // unreachable: the termination arm never cancels
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE6, chargedAny: true,
            promoted: promoted, atCap: atCap },
        }
      },
    },
    now' = now, physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// ReportAttemptOutcome, DeadlineExceeded reason: the controller's
  /// activeDeadlineSeconds kill fills the open attempt row as a timeout.
  /// Under the cap the timeout budget is charged; at the cap the verdict
  /// is terminal Cancelled — the same terminal the worker-reported E4
  /// produces (the D1-adjudicated channel invariance, unchanged by the
  /// pull re-key).
  // Mirrors: admin ReportAttemptOutcome -> the scheduler fill for
  //          DeadlineExceeded.
  action pullAttemptOutcomeDeadline(n: str): bool = all {
    freeCloseRun' = 0,
    match attempt { | PodDead(p) => p.exec == n and p.kind == DkDeadline | _ => false },
    execs.get(n).ctrl == CtrlDeadline,
    val e = execs.get(n)
    val applied = specApply(spec, FControllerDeadlineExceeded(now), eligibleFleet)
    all {
      execs' = execs.put(n, { ...e, runningOurs: false, ctrl: NoCtrl,
        deathCharges: e.deathCharges + 1 }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      live' = applied.c,
      match applied.v {
        | VCancel => all {
          dStatus' = DCancelled,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, status: PgCancelled, ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OE7, terminal: true },
        }
        | VRequeue => all {
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE7, chargedAny: true },
        }
        | VPoison(r) => all {
          // unreachable: the deadline arm never poisons post-collapse
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OE7, chargedAny: true },
        }
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  /// ReportAttemptOutcome with no open attempt to consume it: the report
  /// targets an attempt row that is already terminal (closed by the
  /// worker report, the deadline fill, or the establishment — including
  /// after the derivation re-pulled on the same or another source) or
  /// that never existed (a pod that died before its first pull). The
  /// no-attempt no-op rule: acknowledged, nothing charged, nothing
  /// created, no derivation state touched.
  // Mirrors: the ReportAttemptOutcome no-attempt / already-filled arms
  //          (sched.attempt.no-attempt-no-op).
  action pullAttemptOutcomeNoOp(n: str): bool = all {
    freeCloseRun' = freeCloseRun,
    execs.get(n).ctrl != NoCtrl,
    // the pending report's own attempt is no longer open: anything but a
    // PodDead attempt on this source means the row it targets is closed
    // (a Running/Reported attempt on the same source is a NEWER pull).
    match attempt { | PodDead(p) => p.exec != n | _ => true },
    execs' = execs.put(n, { ...execs.get(n), ctrl: NoCtrl }),
    last' = { ...noObs, kind: OE6, dropped: true },
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, lastPoisonReason' = lastPoisonReason,
    justFailedOver' = false, everFailedBuilders' = everFailedBuilders,
  }

  /// Bughunt wave A2 (bug_279): the worker-supplied Cancelled report
  /// (the AD5 unsolicited abort) admitted UNCHARGED by the kernel —
  /// the attempt closes charge-free (a build-lane Disconnected/Worker
  /// row that no budget fold counts) and the derivation requeues.
  /// Admission is the kernel's trailing-run gate: strictly below
  /// WORKER_ABORT_FREE_CLOSES; at the bound this action's guard closes
  /// and the worker's next abort can only take the charged report
  /// classes (the fall-through).
  // Mirrors: rio-retry-kernel admit_worker_abort (lib.rs) consumed by
  //          the report intake's witness-first AD5 arm
  //          (r[impl sched.attempt.worker-abort-bounded] sites).
  action pullWorkerAbort(n: str): bool = all {
    attempt == Running(n),
    dStatus == DRunning,
    freeCloseRun < WORKER_ABORT_FREE_CLOSES,
    attempt' = NoAttempt,
    dStatus' = DReady,
    execs' = execs.put(n, { ...execs.get(n), runningOurs: false }),
    freeCloseRun' = freeCloseRun + 1,
    // charge-free: no fold event, no counter movement, no backoff.
    spec' = spec, specVerdict' = specVerdict, live' = live,
    pg' = pg, last' = noObs,
    now' = now, poisonSrc' = poisonSrc,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// The establishment sweep: an open attempt whose deadline +
  /// report-slack elapsed with no terminal row is established as
  /// executor_crash/unreported and charged (failed_builders[source] +
  /// failure_count + threshold re-check), closing the attempt and
  /// requeueing or poisoning in the same transaction. The clock gate is
  /// over-approximated away (see the header); the establishment may
  /// therefore race the controller fill and the in-flight worker report
  /// here — whichever lands first closes the row, the loser is the
  /// no-attempt no-op.
  // Mirrors: actor/housekeeping.rs establishment sweep for open
  //          pull-mode attempts (sched.attempt.establishment-window's
  //          charge half; the window timing itself is not modeled).
  action pullEstablish(n: str): bool = all {
    freeCloseRun' = 0,
    match attempt {
      | NoAttempt => false
      | Running(w) => w == n
      | Reported(r) => r.exec == n
      | PodDead(p) => p.exec == n
      | Wedged(w) => w == n
    },
    dStatus == DRunning,
    val e = execs.get(n)
    // the charge counts against the pod's death only if the pod actually
    // died unreported (the PodDead shape); establishing a still-running
    // or already-reported attempt charges the budget but there is no
    // death to double-count.
    val isDeath = match attempt { | PodDead(p) => true | _ => false }
    val applied = specApply(spec, FEstablishedCrash({ at: now, exec: n }), eligibleFleet)
    all {
      execs' = execs.put(n, { ...e, runningOurs: false,
        deathCharges: if (isDeath) e.deathCharges + 1 else e.deathCharges }),
      attempt' = NoAttempt,
      spec' = applied.c,
      specVerdict' = applied.v,
      live' = applied.c,
      match applied.v {
        | VPoison(r) => all {
          dStatus' = DPoisoned,
          poisonSrc' = EventPoison,
          lastPoisonReason' = r,
          pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now), ledger: applied.c },
          depStatus' = DepFailed,
          everFailureTerminal' = true,
          last' = { ...noObs, kind: OEstablish, chargedAny: true, terminal: true },
        }
        | VRequeue => all {
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OEstablish, chargedAny: true },
        }
        | VCancel => all {
          // unreachable: the establishment arm never cancels
          dStatus' = DReady,
          poisonSrc' = poisonSrc,
          lastPoisonReason' = lastPoisonReason,
          pg' = { ...pg, ledger: applied.c },
          depStatus' = depStatus,
          everFailureTerminal' = everFailureTerminal,
          last' = { ...noObs, kind: OEstablish, chargedAny: true },
        }
      },
    },
    now' = now, promotionsLeft' = promotionsLeft, physAttempts' = physAttempts,
    failoversLeft' = failoversLeft, txFaultsLeft' = txFaultsLeft,
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders.union(Set(n)),
  }

  /// The spawn-intent gate's exhausted-universe verdict with no new
  /// charge: every spawnable source is already excluded for this
  /// derivation (the universe shrank under an existing exclusion set),
  /// so the controller reports NoEligibleSource instead of spawning an
  /// unschedulable Job and the scheduler maps it to the same
  /// fleet-exhaust poison the dispatch-time backstop produced (no fold
  /// event — the marker row is a fold no-op). The empty universe defers
  /// instead (the action is disabled), per
  /// sched.dispatch.fleet-exhaust+4's empty-fleet arm.
  // Mirrors: pool/jobs.rs spawn pass (excluded ⊇ spawnable ->
  //          ReportAttemptOutcome(NoEligibleSource)) -> actor/pull.rs
  //          fleet-exhaust mapping (still-Ready only).
  action spawnGateExhaust = all {
    freeCloseRun' = freeCloseRun,
    dStatus == DReady,
    backoffElapsed,
    attempt == NoAttempt,
    exhaustsFleet(live.failedBuilders, spawnableSources),
    dStatus' = DPoisoned,
    poisonSrc' = DispatchPoison,
    live' = { ...live, poisonedAt: At(now) },
    lastPoisonReason' = RFleetExhausted,
    pg' = { ...pg, status: PgPoisoned, poisonedAt: At(now) },
    depStatus' = DepFailed,
    everFailureTerminal' = true,
    spec' = spec, specVerdict' = specVerdict,
    attempt' = attempt, execs' = execs,
    now' = now, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft,
    last' = { ...noObs, kind: OE9Dispatch, terminal: true },
    justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// A source node leaves the spawnable universe (cordon, consolidation,
  /// node repair, pool shrink): the controller's informer view no longer
  /// offers it, so the spawn gate's universe shrinks. Only a source not
  /// hosting our open attempt leaves here — the node dying under a
  /// running build is the pod-death event, already modeled.
  action sourceLeavesPool(n: str): bool = all {
    freeCloseRun' = freeCloseRun,
    execs.get(n).registered,
    not(execs.get(n).runningOurs),
    execs' = execs.put(n, { ...execs.get(n), registered: false }),
    now' = now, dStatus' = dStatus, live' = live, poisonSrc' = poisonSrc,
    spec' = spec, specVerdict' = specVerdict, pg' = pg,
    depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
    attempt' = attempt, promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts, failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft, last' = last,
    lastPoisonReason' = lastPoisonReason, justFailedOver' = false,
    everFailedBuilders' = everFailedBuilders,
  }

  /// Leader failover, pull-mode shape: the cached counter view is lost
  /// and rebuilt by the fold over the durable attempt ledger
  /// (sched.retry.failover-budget,
  /// sched.retry.recovery-projection+3) — but the open pull-mode
  /// attempt SURVIVES the failover (the assignments/executions rows
  /// are durable; the establishment sweep — never a recovery-time
  /// reconcile — owns their repair), and the controller's pending
  /// pod-terminal report obligation is controller-side state that a
  /// scheduler failover does not erase. Nothing is forgiven, nothing
  /// is fabricated, and the open attempt is not re-dispatched out from
  /// under its pod.
  // Mirrors: actor/mod.rs LeaderLost wipe + recovery.rs ledger-fold
  //          rebuild; open attempts come up behind the new leader
  //          untouched and the establishment sweep is their only
  //          time-based repair (sched.attempt.establishment-window+3).
  action pullLeaderFailover = all {
    freeCloseRun' = freeCloseRun,
    failoversLeft > 0,
    val gone = dStatus == DAbsent or dStatus == DCompleted
    val expiredInDowntime = and {
      pg.status == PgPoisoned,
      match pg.poisonedAt { | NoTime => false | At(t) => now - t > POISON_TTL },
    }
    val recovered = { ...pg.ledger, poisonedAt: pg.poisonedAt }
    all {
      failoversLeft' = failoversLeft - 1,
      execs' = execs,
      attempt' = attempt,
      justFailedOver' = true,
      last' = { ...noObs, kind: OFailover },
      if (gone) all {
        dStatus' = dStatus, live' = live, spec' = spec, specVerdict' = specVerdict,
        pg' = pg, poisonSrc' = poisonSrc, depStatus' = depStatus,
        everFailureTerminal' = everFailureTerminal, lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned and expiredInDowntime) all {
        // the TTL elapsed while no leader was watching: clear in PG, do
        // not reload (same as the stream-path failover arm)
        dStatus' = DAbsent, live' = zeroCounters, spec' = zeroCounters,
        specVerdict' = VRequeue, pg' = freshPg, poisonSrc' = NoPoison,
        depStatus' = depStatus, everFailureTerminal' = everFailureTerminal,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgPoisoned) all {
        dStatus' = DPoisoned,
        live' = recovered,
        spec' = pg.ledger,
        specVerdict' = specVerdict,
        pg' = pg,
        poisonSrc' = if (poisonSrc == NoPoison) EventPoison else poisonSrc,
        depStatus' = DepFailed,
        everFailureTerminal' = true,
        lastPoisonReason' = lastPoisonReason,
      } else if (pg.status == PgNonTerminal) all {
        // the retry view is the ledger fold; an open pull attempt keeps
        // the derivation Running (the orphan-reconcile carve-out), a
        // closed one comes back Ready.
        dStatus' = if (attempt == NoAttempt) DReady else DRunning,
        live' = recovered,
        spec' = pg.ledger,
        specVerdict' = specVerdict,
        pg' = pg,
        poisonSrc' = NoPoison,
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
    txFaultsLeft' = txFaultsLeft, everFailedBuilders' = everFailedBuilders,
  }

  // --- step (pull-mode alphabet) -------------------------------------------

  action pullOpensAttemptAny = { nondet n = EXECUTORS.oneOf() pullOpensAttempt(n) }
  action pullReportOutcomeAny = {
    nondet n = EXECUTORS.oneOf()
    nondet class = REPORT_CLASSES.oneOf()
    nondet dual = BOOLS.oneOf()
    nondet promoted = BOOLS.oneOf()
    nondet atCap = BOOLS.oneOf()
    pullReportOutcome(n, class, dual, promoted, atCap)
  }
  action pullAttemptOutcomeOomAny = {
    nondet n = EXECUTORS.oneOf()
    nondet promoted = BOOLS.oneOf()
    nondet atCap = BOOLS.oneOf()
    pullAttemptOutcomeOom(n, promoted, atCap)
  }
  action pullAttemptOutcomeDeadlineAny = { nondet n = EXECUTORS.oneOf() pullAttemptOutcomeDeadline(n) }
  action pullAttemptOutcomeNoOpAny = { nondet n = EXECUTORS.oneOf() pullAttemptOutcomeNoOp(n) }
  action pullWorkerAbortAny = { nondet n = EXECUTORS.oneOf() pullWorkerAbort(n) }
  action pullEstablishAny = { nondet n = EXECUTORS.oneOf() pullEstablish(n) }
  action sourceLeavesPoolAny = { nondet n = EXECUTORS.oneOf() sourceLeavesPool(n) }

  // --- 11. the materialization attempt class ------------------------------
  // (substitution-replacement Phase A, T-5.1; design §2.5/§9.2, OQ1
  // amendments 1-2)
  //
  // Models the kind partition the substitution-replacement campaign adds
  // to the attempt ledger: materialization attempts (store-replica
  // executed, drv_executions.attempt_kind = 'materialization') charge
  // their own bounded budget and are INVISIBLE to every build budget;
  // build attempts never charge the materialization budget. The class is
  // carried as the FOLD of the materialization-kind ledger (bounded
  // counters) — the same representation the build ledger uses
  // (live/spec/pg.ledger) — never as a row list (the SC-1
  // no-unbounded-variables rule: every variable below is ceilinged by an
  // action-precondition guard, and the ceilings are themselves checked by
  // boundsOK).
  //
  // ENCODING NOTE (the partition-tripwire style). The natural-language
  // forms of the two partition invariants are counterfactual ("charging
  // the materialization budget never changes the build verdict"), which a
  // state invariant cannot express directly: the partition makes
  // (build x materialization) state combinations a full product, so every
  // post-state of a leaking edit is also reachable legitimately and no
  // predicate over the two sides alone can distinguish them. The
  // invariants are therefore encoded over a pre-state snapshot ghost:
  // every materialization action captures the build-side budget view it
  // observed (matObservedBuild), every build-side action clears it, and
  // the invariants assert the post-state build view equals the captured
  // one. A materialization action edited to write any build-side budget
  // variable falsifies them in that very state (the
  // materialization->build tripwire direction); a build-side action
  // edited to bump a materialization counter overruns the boundsOK
  // ceilings within one regime's attempt budget (the build->
  // materialization direction). Both directions are calibrated by
  // working-tree falsification before wiring (the introducing commit
  // records the transcripts).

  /// The materialization budget (production: materialization.max_attempts,
  /// scaled per the model's budget-scaling discipline — every cap stays
  /// reachable in a small state space; the witness proves the park state
  /// is reached).
  pure val MAX_MATERIALIZATION_ATTEMPTS = 2
  /// Ceiling on modeled unobtainable reports. Unobtainable is a routing
  /// verdict (the four-arm consumption), not a retry: it never charges
  /// the infra budget and never counts toward parking, so it needs its
  /// OWN ceiling to keep the state space finite (review finding SC-1 —
  /// matParked alone would not bound it).
  pure val MAX_MAT_UNOBTAINABLE = 1

  /// The build-side budget view a materialization action observes (the
  /// subject of the two partition invariants): the cached counters, the
  /// reference-fold ghost, the durable row, the verdict, the derivation /
  /// dependent / attempt status, the poison bookkeeping and the dispatch
  /// ghost. Environment bookkeeping (the clock, executor slots, the floor
  /// ladder, the failover/tx budgets, the last-observation ghost) is
  /// deliberately not captured: the partition claim is about budgets,
  /// verdicts and poison/cascade state, and pullStep's composition
  /// already forces materialization actions to leave every build-side
  /// variable unchanged.
  type BuildView = {
    dStatus: DStatus,
    live: Counters,
    spec: Counters,
    specVerdict: Verdict,
    pg: PgRow,
    depStatus: DepStatus,
    everFailureTerminal: bool,
    poisonSrc: PoisonSrc,
    lastPoisonReason: Reason,
    attempt: Attempt,
    physAttempts: int,
    everFailedBuilders: Set[str],
  }
  type BuildSnap = | NoBuildSnap | CapturedAt(BuildView)

  /// materialization_infra rows charged to the modeled job (worker-
  /// reported AND establishment-written — both charge this one budget,
  /// OQ1 amendment 1).
  var matInfraCount: int
  /// The establishment-written (charging party = scheduler) subset of
  /// matInfraCount — carried separately so the crash-channel witness is
  /// expressible.
  var matSchedulerInfraCount: int
  /// materialization_unobtainable rows (routing verdicts).
  var matUnobtainableCount: int
  /// GHOST: the build-side budget view captured by the most recent
  /// materialization action; NoBuildSnap after every build-side action.
  /// The two partition invariants compare it against the live build
  /// state.
  var matObservedBuild: BuildSnap

  /// The modeled materialization job is parked (budget exhausted). Park
  /// is the materialization analogue of a terminal budget verdict: the
  /// attempt actions stop (their guards close), nothing is poisoned —
  /// materializationNeverPoisons is checked at every budget level
  /// including this one.
  def matParked: bool = matInfraCount >= MAX_MATERIALIZATION_ATTEMPTS

  /// The current build-side budget view (the snapshot constructor).
  def currentBuildView: BuildView = {
    dStatus: dStatus,
    live: live,
    spec: spec,
    specVerdict: specVerdict,
    pg: pg,
    depStatus: depStatus,
    everFailureTerminal: everFailureTerminal,
    poisonSrc: poisonSrc,
    lastPoisonReason: lastPoisonReason,
    attempt: attempt,
    physAttempts: physAttempts,
    everFailedBuilders: everFailedBuilders,
  }

  // --- 11b. the partition invariants + witness ----------------------------

  /// materializationNeverPoisons (design §2.5; sched.materialize.routing's
  /// never-poison clause): no materialization-kind charge — worker-
  /// reported Unobtainable or InfraFailure, or the establishment-written
  /// crash charge (OQ1 amendment 1) — ever produces a Poison or Cancel
  /// verdict or touches the cascade, at any budget level including
  /// exhaustion (parking). State-invariant form: in the state right after
  /// any materialization action, the derivation's terminal status, poison
  /// bookkeeping, durable status and cascade state are exactly what the
  /// action observed before it.
  val materializationNeverPoisons = match matObservedBuild {
    | NoBuildSnap => true
    | CapturedAt(v) => and {
        dStatus == DPoisoned implies v.dStatus == DPoisoned,
        dStatus == DCancelled implies v.dStatus == DCancelled,
        poisonSrc == v.poisonSrc,
        lastPoisonReason == v.lastPoisonReason,
        pg.status == v.pg.status,
        depStatus == v.depStatus,
        everFailureTerminal == v.everFailureTerminal,
      }
  }

  /// materializationInvisibleToBuildBudgets (design §2.5; OQ1 amendment 2,
  /// the typed carve-out): materialization charges feed exactly one
  /// budget — their own — and are invisible to every build budget.
  /// State-invariant form (the countersRefineHistory tripwire style): in
  /// the state right after any materialization action, every build-side
  /// budget view — the cached counters, the reference fold, the durable
  /// ledger fold, the fold verdict, the open attempt and the dispatch
  /// ghost — equals the view the action observed. A materialization
  /// charge that leaks into any build fold falsifies this in that very
  /// state.
  val materializationInvisibleToBuildBudgets = match matObservedBuild {
    | NoBuildSnap => true
    | CapturedAt(v) => and {
        live == v.live,
        spec == v.spec,
        specVerdict == v.specVerdict,
        pg == v.pg,
        dStatus == v.dStatus,
        attempt == v.attempt,
        physAttempts == v.physAttempts,
        everFailedBuilders == v.everFailedBuilders,
      }
  }

  /// Violated when a crash establishment has charged the materialization
  /// budget (establishMaterializationCrash is reachable and counted) —
  /// the OQ1-amendment-1 channel exists in the model and feeds the
  /// materialization budget, never the build one. Wired as the
  /// expect-violation check
  /// quint-retry-policy-pull-witness-materialization-crash.
  val noMaterializationCrashCharge = not(matSchedulerInfraCount > 0)

  // --- 11c. the materialization actions ------------------------------------

  /// Build-side step disjuncts leave the materialization attempt class
  /// untouched and clear the snapshot ghost (the build->materialization
  /// direction of the kind partition). Composed with every build-side
  /// disjunct of pullStep; the build actions themselves are untouched.
  action buildLeavesMatUntouched = all {
    matInfraCount' = matInfraCount,
    matSchedulerInfraCount' = matSchedulerInfraCount,
    matUnobtainableCount' = matUnobtainableCount,
    matObservedBuild' = NoBuildSnap,
  }

  /// Materialization actions leave every build-side variable untouched
  /// (the materialization->build direction of the kind partition; the
  /// snapshot the partition invariants compare against is captured by the
  /// calling action). justFailedOver and last are preserved, not cleared:
  /// a materialization action is invisible to the build side, so the
  /// recovery-invariant scoping and the per-event ghosts must read
  /// exactly as if it had not happened.
  action matLeavesBuildUntouched = all {
    freeCloseRun' = freeCloseRun,
    now' = now,
    dStatus' = dStatus,
    live' = live,
    poisonSrc' = poisonSrc,
    spec' = spec,
    specVerdict' = specVerdict,
    pg' = pg,
    depStatus' = depStatus,
    everFailureTerminal' = everFailureTerminal,
    attempt' = attempt,
    execs' = execs,
    promotionsLeft' = promotionsLeft,
    physAttempts' = physAttempts,
    everFailedBuilders' = everFailedBuilders,
    failoversLeft' = failoversLeft,
    txFaultsLeft' = txFaultsLeft,
    last' = last,
    lastPoisonReason' = lastPoisonReason,
    justFailedOver' = justFailedOver,
  }

  /// A store replica's materialization attempt reports Unobtainable: a
  /// routing verdict consumed by the four-arm consumption transaction,
  /// not a retry — it never charges the infra budget and never counts
  /// toward parking, so it carries its own ceiling.
  // Mirrors: rio-store materialize executor reporting
  //          MaterializationOutcome::Unobtainable -> the scheduler
  //          consumption transaction's routing arms (T-3.5).
  action materializationReportUnobtainable = all {
    ENABLE_MATERIALIZATION,
    matUnobtainableCount < MAX_MAT_UNOBTAINABLE,
    matUnobtainableCount' = matUnobtainableCount + 1,
    matInfraCount' = matInfraCount,
    matSchedulerInfraCount' = matSchedulerInfraCount,
    matObservedBuild' = CapturedAt(currentBuildView),
    matLeavesBuildUntouched,
  }

  /// A store replica's materialization attempt reports InfraFailure: the
  /// worker-reported channel of the materialization budget.
  // Mirrors: MaterializationOutcome::InfraFailure -> the consumption
  //          transaction's budget charge (rio-retry-kernel
  //          materialization_decide).
  action materializationReportInfra = all {
    ENABLE_MATERIALIZATION,
    not(matParked),
    matInfraCount' = matInfraCount + 1,
    matSchedulerInfraCount' = matSchedulerInfraCount,
    matUnobtainableCount' = matUnobtainableCount,
    matObservedBuild' = CapturedAt(currentBuildView),
    matLeavesBuildUntouched,
  }

  /// The establishment sweep charges a crashed store replica's open
  /// materialization attempt as materialization_infra — never
  /// executor_crash (OQ1 amendment 1 / finding PP-4: the channel that
  /// could violate the partition, modeled so the partition invariants are
  /// non-vacuous against it). It charges the SAME budget as the
  /// worker-reported infra channel; the scheduler-written subset is
  /// carried separately for the witness.
  // Mirrors: actor/housekeeping.rs establishment sweep, the
  //          materialization branch (no adopt arm, materialization_infra
  //          charge, never executor_crash).
  action establishMaterializationCrash = all {
    ENABLE_MATERIALIZATION,
    not(matParked),
    matInfraCount' = matInfraCount + 1,
    matSchedulerInfraCount' = matSchedulerInfraCount + 1,
    matUnobtainableCount' = matUnobtainableCount,
    matObservedBuild' = CapturedAt(currentBuildView),
    matLeavesBuildUntouched,
  }

  // --- 12. the pull-mode transition relation -------------------------------

  /// The pull-mode transition relation. The physical attempt-end events
  /// (buildEndsReported / buildEndsPodDeath) and the lost controller
  /// report are the shared environment actions; the resets and the
  /// clock are composed unchanged; everything channel-shaped is the
  /// pull-path encoding above. The as-built `step` (dispatch stream,
  /// disconnect, correlation channels, backstop, respawn) is not
  /// reachable from here.
  ///
  /// Every build-side disjunct is composed with buildLeavesMatUntouched
  /// (the kind partition, build->materialization direction); the build
  /// actions' own text is unchanged. The three materialization disjuncts
  /// are enabled only in regimes with ENABLE_MATERIALIZATION = true and
  /// leave the build side untouched (the materialization->build
  /// direction). In ENABLE_MATERIALIZATION = false regimes the
  /// materialization variables are frozen at their init values, so the
  /// reachable state space is bit-identical to the pre-Phase-A one.
  action pullStep = any {
    all { tick, buildLeavesMatUntouched },
    all { pullOpensAttemptAny, buildLeavesMatUntouched },
    all { spawnGateExhaust, buildLeavesMatUntouched },
    all { buildEndsReportedAny, buildLeavesMatUntouched },
    all { buildEndsPodDeathAny, buildLeavesMatUntouched },
    all { pullReportOutcomeAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeOomAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeDeadlineAny, buildLeavesMatUntouched },
    all { pullAttemptOutcomeNoOpAny, buildLeavesMatUntouched },
    all { pullWorkerAbortAny, buildLeavesMatUntouched },
    all { ctrlReportLostAny, buildLeavesMatUntouched },
    all { pullEstablishAny, buildLeavesMatUntouched },
    all { sourceLeavesPoolAny, buildLeavesMatUntouched },
    all { resubmitReset, buildLeavesMatUntouched },
    all { cacheHitClear, buildLeavesMatUntouched },
    all { poisonTtlExpires, buildLeavesMatUntouched },
    all { pullLeaderFailover, buildLeavesMatUntouched },
    // The materialization attempt class (substitution-replacement
    // Phase A; disabled when ENABLE_MATERIALIZATION = false).
    materializationReportUnobtainable,
    materializationReportInfra,
    establishMaterializationCrash,
  }
}

// ===========================================================================
// REGIME MODULES — each is the --main of one or more wired checks in
// nix/quint.nix; each instantiates the core with that regime's constants
// (the one-model-N-regimes discipline). Measured state counts, depths and
// wall-clocks live in the introducing commits' messages and the checks'
// transcripts, never here.
// ===========================================================================

/// The pull-mode environment regime (executor-lifecycle campaign, slice
/// 1b; the wired set since the 1c' retirement of the as-built-channel
/// regimes). ENABLE_MATERIALIZATION = false: the materialization attempt
/// class is dormant here, so this regime's reachable state space is
/// bit-identical to the pre-Phase-A pull regime — the dormancy criterion
/// the substitution-replacement campaign's wired-check invariance gate
/// re-proves at every Phase A commit. Checks: quint-retry-policy-pull and
/// the nine quint-retry-policy-pull-witness-* expect-violation probes.
module retryPolicyPull {
  import retryPolicy(
    EXECUTORS = Set("n1", "n2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 6,
    ATTEMPT_BOUND = 6,
    MAX_FAILOVERS = 1,
    TX_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = true,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = true,
    ENABLE_MATERIALIZATION = false
  ).*
}

/// The materialization-coexistence regime (substitution-replacement
/// Phase A, T-5.1/T-5.2): the materialization attempt class enabled
/// alongside the build channels the kind partition is about — the worker
/// report charge channel (E1-E4), the no-report crash death and its
/// build-side establishment (the OQ1-amendment-1 adjacency: BOTH
/// establishment channels are reachable here, and the partition
/// invariants pin that the materialization one never feeds the build
/// fold), dispatch, the spawn-gate exhaust and the source-universe
/// shrink. The controller-fill machinery (ENABLE_PODDEATH), the resets
/// and leader failover are the build-only regime's concern: they explore
/// build-internal lifecycles the materialization class is structurally
/// independent of, and excluding them keeps this regime's product state
/// space inside the merge-gate budget (the SC-1 wall-clock thresholds).
/// Checks: quint-retry-policy-pull-materialization (exhaustive, the full
/// invariant list including the two partition invariants) and
/// quint-retry-policy-pull-witness-materialization-crash.
module retryPolicyPullMat {
  import retryPolicy(
    EXECUTORS = Set("n1", "n2"),
    MAX_TIME = 3,
    MAX_ATTEMPTS = 6,
    ATTEMPT_BOUND = 6,
    MAX_FAILOVERS = 0,
    TX_FAULTS = 0,
    ENABLE_WORKER = true,
    ENABLE_PODDEATH = false,
    ENABLE_CRASH = true,
    ENABLE_WEDGE = false,
    ENABLE_RESETS = false,
    ENABLE_MATERIALIZATION = true
  ).*
}
