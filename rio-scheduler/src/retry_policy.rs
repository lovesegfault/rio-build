//! The reference fold over a derivation's observed failure history.
//!
//! This is the retry-formal campaign's Phase-0 specification oracle for
//! the retry/poison/cascade machinery.
//!
//! [`reference_fold`] is a pure function from an observed failure-event
//! history to the ten `RetryState` counters and the budget verdict. It is
//! the executable specification of what the seventeen `RetryState`
//! mutation sites and nine cap-check entry points (E1–E9 in
//! `docs/spec/models/retry-invariant-map.md`) collectively implement:
//! which event charges which counter, the 300 s sliding-window reset, the
//! resource-floor `{promoted, at_cap}` exemption, the cache-hit and
//! resubmit resets (as explicit history events), the per-executor
//! exclusion set, and the budget verdicts (requeue / poison / cancel /
//! TTL-expire).
//!
//! **The fold became load-bearing in Phase 1b.** [`reference_fold`]
//! itself stays the executable specification (and the model's oracle);
//! the production decision surface layered on top of it is
//! [`decide`] / [`classify`] / [`placeable`] (the design's §5a-2
//! contract), which all nine entry points call (T-1b.2..T-1b.13). Since
//! the T-1b.13 retirement no site mutates a `RetryState` counter in
//! place: the cached dispatch view is refreshed from this fold whenever
//! a node's attempt history changes, so `CountersRefineHistory` holds by
//! construction (modulo the documented `poisoned_at`/`backoff_until`
//! carve-outs and the dag-merge resubmit-cycle carry).
//!
//! ## Phase-1 scope notes (P3/P4, recorded here per the Phase-1 plan)
//!
//! - **P3 (transient per-cycle cap):** `decide()`'s transient arm keeps
//!   the `max_retries` cap (`PoisonReason::TransientBudget`). Under
//!   production defaults the arm is shadowed by the distinct-worker
//!   poison threshold (see the comment at the cap check in [`apply`]);
//!   it stays because `sched.retry.transient-budget`'s final clause
//!   mandates it and non-distinct/dev configurations still reach it.
//! - **P4 (floor-promotion exemption, the c13f6a277 / I-213 class):**
//!   the exemption is infra-class only. [`classify`] maps a
//!   worker-reported infra failure with `floor_outcome.promoted` or a
//!   CONCURRENT_PUTPATH message — and a promoted controller
//!   termination — to the exempt infra class; a `TransientFailure`
//!   classifies as `transient` with **no floor outcome consulted**, and
//!   the transient arm carries no exemption (the as-built
//!   `handle_transient_failure` has no floor/promotion guard).
//!   Regression coverage for the promotion-exempt ladder stays with the
//!   existing `sched.retry.promotion-exempt+3` unit tests
//!   (`test_transient_failure_promotion_exempt_from_max_retries`); the
//!   floor oracle is NOT extended to transient events and the model
//!   deliberately does not encode one (NOT-ENC).
//!
//! ## What "the history" is
//!
//! The input is the **observed** accounting-event sequence — one entry per
//! entry-point invocation that the dedup layer let through, in the order
//! the single-threaded actor processed them — not the physical attempt
//! history. The physical → observed projection (one pod death fanning out
//! into up to four channel observations, the `recently_disconnected` /
//! `last_completed` dedup deciding which of them count) is the
//! environment's nondeterminism; the Stage-B model quantifies over it and
//! checks that the code's counters equal `reference_fold(observed)`
//! (`r[sched.retry.counters-refine-history+2]`) and that the verdict is
//! the same for every observation of one physical history
//! (`r[sched.retry.verdict-channel-invariant]`).
//!
//! ## Where the fold deliberately deviates from the code
//!
//! Per the Phase-0 plan, every place two entry points disagree such that
//! no single channel-invariant fold can reproduce both is a DIVERGENCE row
//! in the invariant map; the fold implements the side the spec mandates
//! (or the side judged intended where the spec is silent) and the other
//! side is the deviation Phase 1 must disposition. The deviations are
//! marked `DIVERGENCE Dn` inline below; `CountersRefineHistory` is
//! *expected* to falsify on histories that reach them. Everywhere else the
//! fold reproduces the code exactly, including its per-counter fencepost
//! conventions and its per-event-class asymmetries.
//!
//! ## Conventions
//!
//! - Time is an abstract monotonic clock in whole seconds ([`AbsTime`]).
//!   `std::time::Instant` is deliberately not used: the fold must be
//!   constructible at arbitrary points for hand-computed histories, and
//!   the only consumers of real time are the 300 s infra window, the 24 h
//!   poison TTL, and the backoff deadline.
//! - The backoff is the deterministic curve `min(base · multᵃ, cap)`
//!   without the production ±jitter; the model compares `backoff_until`
//!   modulo the jitter spread.
//! - Executor identities are plain `String`s so the fold core stays a
//!   leaf (no dependency on the actor, the DAG, the state machine, or
//!   tokio). The Phase-1b decision surface ([`decide`] and friends)
//!   consumes the state module's plain data vocabulary
//!   ([`AttemptRecord`], [`OutcomeClass`], [`ExecutorId`]) and maps it
//!   onto the fold's String-based events; it still has no actor, DAG, or
//!   tokio dependency.
//! - The fold assumes the derivation is in a dispatchable, non-terminal
//!   state when each event arrives — the entry points' "is the node still
//!   poison-able" status guards are upstream of the accounting, and an
//!   event that was dropped by such a guard is simply absent from the
//!   observed history.

use std::collections::BTreeSet;

use crate::state::{AttemptEventKind, AttemptRecord, ExecutorId, OutcomeClass, ReportingParty};

/// Abstract monotonic clock, in whole seconds since an arbitrary origin.
pub(crate) type AbsTime = u64;

/// The retry/poison budget — the union of `RetryPolicy`, `PoisonConfig`,
/// `POISON_RESUBMIT_RETRY_LIMIT`, and `POISON_TTL`, flattened into one
/// plain struct so the fold has a single configuration argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Budget {
    /// `RetryPolicy.max_retries` — the per-cycle transient cap.
    pub max_retries: u32,
    /// `RetryPolicy.max_infra_retries` — the non-exempt infra cap.
    pub max_infra_retries: u32,
    /// `RetryPolicy.max_timeout_retries` — the timeout cap.
    pub max_timeout_retries: u32,
    /// `RetryPolicy.max_exempt_infra_retries` — the cap-exemption's own
    /// terminal.
    pub max_exempt_infra_retries: u32,
    /// `RetryPolicy.infra_retry_window_secs` — the sliding-window reset.
    pub infra_retry_window_secs: u64,
    /// `RetryPolicy.backoff_base_secs` (whole seconds).
    pub backoff_base_secs: u64,
    /// `RetryPolicy.backoff_multiplier` (integral; the production default
    /// is 2.0).
    pub backoff_multiplier: u64,
    /// `RetryPolicy.backoff_max_secs` (whole seconds).
    pub backoff_max_secs: u64,
    /// `PoisonConfig.threshold`.
    pub poison_threshold: u32,
    /// `PoisonConfig.require_distinct_workers` — `true` counts distinct
    /// members of `failed_builders`; `false` counts `failure_count`.
    pub require_distinct_workers: bool,
    /// `POISON_RESUBMIT_RETRY_LIMIT` — how many resubmit resets a
    /// `Poisoned` node gets before it sticks.
    pub poison_resubmit_retry_limit: u32,
    /// `POISON_TTL` in seconds (24 h in production).
    pub poison_ttl_secs: u64,
}

impl Default for Budget {
    /// The production defaults (`RetryPolicy::default()`,
    /// `PoisonConfig::default()`, the two consts).
    fn default() -> Self {
        Self {
            max_retries: 2,
            max_infra_retries: 10,
            max_timeout_retries: 4,
            max_exempt_infra_retries: 50,
            infra_retry_window_secs: 300,
            backoff_base_secs: 5,
            backoff_multiplier: 2,
            backoff_max_secs: 300,
            poison_threshold: 3,
            require_distinct_workers: true,
            poison_resubmit_retry_limit: 2,
            poison_ttl_secs: 24 * 60 * 60,
        }
    }
}

impl Budget {
    /// The deterministic backoff curve for retry `attempt` (0-indexed):
    /// `min(base · multᵃ, cap)`, the no-jitter form of
    /// `RetryPolicy::backoff_duration`. E1 computes the backoff from the
    /// count *before* incrementing it, so the first transient retry waits
    /// `base` seconds.
    pub fn backoff_secs(&self, attempt: u32) -> u64 {
        let mut d = self.backoff_base_secs;
        for _ in 0..attempt {
            d = d.saturating_mul(self.backoff_multiplier);
            if d >= self.backoff_max_secs {
                return self.backoff_max_secs;
            }
        }
        d.min(self.backoff_max_secs)
    }
}

/// The live eligible fleet, as the fleet-exhaust check sees it: the set of
/// registered, non-draining executors that are statically eligible for
/// this derivation (kind, system, and required-features match).
///
/// The fleet is an input to the verdict, not part of the history: the
/// fleet-exhaust poison is a function of (exclusion set × live fleet) and
/// is quantified over fleet states by the model, not folded over events
/// (`PlacementIsAFunctionOfExclusionAndFleet` in the design). The fold
/// evaluates the predicate against this one snapshot for every
/// `Transient` event in the history; a test or model run that needs the
/// fleet to change mid-history must split the history at the change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FleetView {
    /// Statically-eligible, non-draining, registered executor ids.
    pub eligible: BTreeSet<String>,
}

/// One observed accounting event. The variants are the nine entry points'
/// triggers plus the reset events and the dispatch (which clears the
/// backoff defer). Every variant carries its observation time `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptEvent {
    /// E1 — worker `CompletionReport{TransientFailure}` (the build ran
    /// and exited non-zero) or `Unspecified`.
    Transient { at: AbsTime, executor: String },
    /// E2 — worker `CompletionReport{InfrastructureFailure}` (FUSE EIO,
    /// cgroup setup failure, CgroupOom) or an unsolicited `Cancelled`.
    ///
    /// `exempt` is the entry point's `exempt_from_cap`: the error message
    /// contains CONCURRENT_PUTPATH, or `bump_resource_floor` returned
    /// `promoted = true` for a CgroupOom. `at_cap` is the floor outcome's
    /// `at_cap` (the relevant dimension is already at its ceiling).
    /// `promoted` and `at_cap` are mutually exclusive; both are false for
    /// non-OOM infra failures and for the cold-start no-intent case.
    Infra {
        at: AbsTime,
        executor: String,
        exempt: bool,
        at_cap: bool,
    },
    /// E3 — one of the seven permanent statuses (`PermanentFailure`,
    /// `CachedFailure`, `DependencyFailed`, `LogLimitExceeded`,
    /// `OutputRejected`, `NotDeterministic`, `InputRejected`).
    Permanent { at: AbsTime, executor: String },
    /// E4 — worker `CompletionReport{TimedOut}`.
    WorkerTimeout { at: AbsTime, executor: String },
    /// E5 — gRPC stream disconnect, heartbeat timeout, or force-drain.
    /// Charges nothing; re-checks the poison threshold over failures
    /// recorded by other events.
    Disconnect { at: AbsTime, executor: String },
    /// E6 — controller `ReportExecutorTermination{OomKilled,
    /// EvictedDiskPressure}`, correlated back to this derivation through
    /// `recently_disconnected` (or the race-ahead live-executor lookup).
    /// `promoted` / `at_cap` are `bump_resource_floor`'s outcome for the
    /// reported dimension.
    ControllerTermination {
        at: AbsTime,
        executor: String,
        promoted: bool,
        at_cap: bool,
    },
    /// E7 — controller `ReportExecutorTermination{DeadlineExceeded}`,
    /// prefix-matched back to this derivation.
    ControllerDeadlineExceeded { at: AbsTime, executor: String },
    /// E8 — the scheduler-side backstop timer: Running for longer than
    /// `max(est × 3, daemon_timeout + slack)` with no report.
    BackstopTimeout { at: AbsTime, executor: String },
    /// The correlation-TTL sweep established a released execution whose
    /// classifying report never arrived (`outcome_class='executor_crash'`,
    /// `termination_reason='unreported'`). Phase 1b (T-1b.11, the C2
    /// adjudication): an established no-report crash charges the
    /// threshold/exclusion budget — `failed_builders[executor]` +
    /// `failure_count`, nothing else (decision P1) — so the no-report
    /// crash loop is bounded by the same budget the existing
    /// `sched.retry.per-executor-budget` "executor disconnect DOES
    /// count" MUST names. A bare `Disconnect` (not yet established)
    /// stays uncharged.
    EstablishedCrash { at: AbsTime, executor: String },
    /// A successful dispatch. Clears `backoff_until`
    /// (`assign_to_worker`).
    Dispatched { at: AbsTime, executor: String },
    /// The `dag::merge` resubmit reset of a retriable terminal node:
    /// fresh per-cycle state, `resubmit_cycles` incremented. The event is
    /// only legal when the node `is_retriable_on_resubmit()` —
    /// `Cancelled`/`Failed`/`DependencyFailed` unconditionally, `Poisoned`
    /// iff `resubmit_cycles < poison_resubmit_retry_limit`; the fold
    /// applies it unconditionally and the model checks the precondition.
    ResubmitReset { at: AbsTime },
    /// The cache-hit reset on a transition out of
    /// `Poisoned`/`DependencyFailed`/`Failed` (the output turned up in
    /// the store or a re-probe found it substitutable). Clears nine of
    /// the ten counters — `backoff_until` survives.
    CacheHitClear { at: AbsTime },
    /// Admin `ClearPoison` or the 24 h TTL expiry: PG cleared
    /// (`resubmit_cycles = 0`), the node removed from the DAG and
    /// re-inserted fresh on the next merge.
    PoisonCleared { at: AbsTime },
}

impl AttemptEvent {
    /// The observation time of this event.
    pub fn at(&self) -> AbsTime {
        match self {
            Self::Transient { at, .. }
            | Self::Infra { at, .. }
            | Self::Permanent { at, .. }
            | Self::WorkerTimeout { at, .. }
            | Self::Disconnect { at, .. }
            | Self::ControllerTermination { at, .. }
            | Self::ControllerDeadlineExceeded { at, .. }
            | Self::BackstopTimeout { at, .. }
            | Self::EstablishedCrash { at, .. }
            | Self::Dispatched { at, .. }
            | Self::ResubmitReset { at }
            | Self::CacheHitClear { at }
            | Self::PoisonCleared { at } => *at,
        }
    }
}

/// The ten `RetryState` counters as the fold computes them. Field-for-
/// field mirror of `crate::state::RetryState` with the two `Instant`
/// fields as [`AbsTime`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// Per-cycle transient retry count (`RetryState::count`).
    pub count: u32,
    /// Cross-cycle resubmit-reset count (`RetryState::resubmit_cycles`).
    pub resubmit_cycles: u32,
    /// Non-exempt infrastructure failure count (`RetryState::infra_count`).
    pub infra_count: u32,
    /// Timeout count (`RetryState::timeout_count`).
    pub timeout_count: u32,
    /// Anchor of the 300 s infra window
    /// (`RetryState::last_infra_failure_at`).
    pub last_infra_failure_at: Option<AbsTime>,
    /// Cap-exempt infrastructure failure count
    /// (`RetryState::exempt_infra_count`).
    pub exempt_infra_count: u32,
    /// The per-executor exclusion set (`RetryState::failed_builders`).
    /// Drives `hard_filter`'s placement exclusion, the distinct-workers
    /// poison threshold, and the fleet-exhaust check.
    pub failed_builders: BTreeSet<String>,
    /// Flat failure count for `require_distinct_workers = false`
    /// (`RetryState::failure_count`).
    pub failure_count: u32,
    /// When the derivation was poisoned (`RetryState::poisoned_at`).
    pub poisoned_at: Option<AbsTime>,
    /// Earliest re-dispatch time (`RetryState::backoff_until`).
    pub backoff_until: Option<AbsTime>,
}

impl Counters {
    /// `PoisonConfig::is_poisoned` — the threshold check over either the
    /// distinct-worker set or the flat counter.
    pub fn poison_threshold_reached(&self, budget: &Budget) -> bool {
        let n = if budget.require_distinct_workers {
            self.failed_builders.len() as u32
        } else {
            self.failure_count
        };
        n >= budget.poison_threshold
    }

    /// The cache-hit reset — wipes nine of the ten fields.
    /// `backoff_until` deliberately survives (the as-built in-place
    /// clear never touched it; since T-1b.13 this arm IS the reset —
    /// the sites append a `cache_hit_clear` row instead of mutating).
    fn clear_for_cache_hit(&mut self) {
        let backoff = self.backoff_until;
        *self = Self {
            backoff_until: backoff,
            ..Self::default()
        };
    }

    // r[impl sched.retry.recovery-projection+2]
    /// The pure legacy projection of the mirror columns — the
    /// pre-ledger-era recovery contract, kept as an executable
    /// definition because it is still load-bearing twice: it is the
    /// degenerate result of the seeded fold for a derivation with an
    /// empty attempt suffix (the `sched.retry.recovery-projection+2`
    /// "pre-ledger fallback" clause), and it is what the as-built
    /// Stage-B model's failover action and the calibration's G8 reverts
    /// encode until the Phase-1c re-encode.
    ///
    /// Since T-1b.12a the live recovery path no longer USES this
    /// projection as the recovered view: recovery runs [`decide`] over
    /// the loaded suffix with the columns as the transitional legacy
    /// seed (P5), so a non-empty suffix contributes everything the
    /// columns never mirrored (the 5 formerly-forgiven counters,
    /// backstop- and crash-established exclusions). The projection is
    /// *not* the fold of the pre-failover history: `failure_count` both
    /// forgets same-worker repeats (the live counter counts them) and
    /// counts the permanent path's diagnostics-only `failed_builders`
    /// insert (the live counter never charged it), and `failed_builders`
    /// itself is missing every backstop-recorded failure (divergence
    /// D4: E8 never mirrors its insert to PG). The no-fabrication bound
    /// is that every recovered value is supported by a persisted column
    /// — nothing is invented.
    pub fn recovery_projection(persisted: &PersistedRetryColumns) -> Self {
        Self {
            count: persisted.retry_count,
            resubmit_cycles: persisted.resubmit_cycles,
            failure_count: persisted.failed_builders.len() as u32,
            failed_builders: persisted.failed_builders.clone(),
            poisoned_at: persisted.poisoned_at,
            ..Self::default()
        }
    }
}

/// The `derivations` columns that mirror retry state, as recovery reads
/// them. `poisoned_at` is `None` for rows loaded by
/// `load_nonterminal_derivations` (which filters out `poisoned`) and for
/// poisoned rows whose TTL already expired during the downtime (recovery
/// clears those instead of reloading them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PersistedRetryColumns {
    /// `derivations.retry_count`.
    pub retry_count: u32,
    /// `derivations.resubmit_cycles`.
    pub resubmit_cycles: u32,
    /// `derivations.failed_builders`.
    pub failed_builders: BTreeSet<String>,
    /// `derivations.poisoned_at`, already converted to the abstract
    /// clock and already filtered for TTL expiry.
    pub poisoned_at: Option<AbsTime>,
}

impl PersistedRetryColumns {
    /// Whether the mirror columns carry any pre-existing retry state at
    /// all. An all-default row contributes nothing to the legacy seed
    /// (decision P5), so [`decide`] skips the floor entirely for it.
    pub(crate) fn is_empty(&self) -> bool {
        self.retry_count == 0
            && self.resubmit_cycles == 0
            && self.failed_builders.is_empty()
            && self.poisoned_at.is_none()
    }
}

/// Which budget's exhaustion produced a `Poison` verdict. The production
/// poison reason is a free-form string (synthesized on some paths,
/// carrying the worker's error message on others — divergence A8); the
/// fold carries the discriminant only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PoisonReason {
    /// `PoisonConfig::is_poisoned` — the distinct-worker / flat-count
    /// threshold.
    Threshold,
    /// Every statically-eligible non-draining worker is in
    /// `failed_builders`.
    FleetExhausted,
    /// `count >= max_retries`.
    TransientBudget,
    /// `infra_count >= max_infra_retries`.
    InfraBudget,
    /// `exempt_infra_count >= max_exempt_infra_retries`.
    ExemptInfraBudget,
    /// A permanent failure status — poisoned directly, no budget.
    Permanent,
}

/// The budget verdict for a derivation given its failure history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The derivation is not terminally locked out by any budget: it is
    /// eligible for (re-)dispatch, possibly deferred until
    /// `Counters::backoff_until`. This is also the verdict for an empty
    /// history and for a history ending in a successful dispatch or a
    /// reset event.
    Requeue,
    /// Terminal `Poisoned`: 24 h TTL, `DependencyFailed` cascade to
    /// dependents, resubmit bounded by `poison_resubmit_retry_limit`.
    Poison(PoisonReason),
    /// Terminal `Cancelled` via timeout-budget exhaustion: the build
    /// still fails and the cascade still runs, but the derivation is
    /// immediately retriable on explicit resubmit (no 24 h lockout).
    Cancel,
    /// The derivation is `Poisoned` and `now - poisoned_at` exceeds the
    /// TTL: the next housekeeping tick clears it (PG clear + DAG
    /// removal).
    TtlExpire,
}

// r[impl sched.dispatch.fleet-exhaust+3]
/// The fleet-exhaust predicate shared by E1's poison check and E9's
/// dispatch-time backstop: every statically-eligible non-draining
/// registered worker has already failed this derivation. Returns `false`
/// when the exclusion set is empty (nothing has failed) or the eligible
/// fleet is empty (no pool is connected — that is a transient the
/// autoscaler handles, and poisoning would brick builds during a
/// rollout).
pub(crate) fn exhausts_fleet(failed_builders: &BTreeSet<String>, fleet: &FleetView) -> bool {
    if failed_builders.is_empty() || fleet.eligible.is_empty() {
        return false;
    }
    fleet.eligible.iter().all(|w| failed_builders.contains(w))
}

// r[impl sched.retry.counters-refine-history+2]
// r[impl sched.retry.transient-budget]
// r[impl sched.retry.attempts-bounded+2]
// r[impl sched.retry.verdict-channel-invariant]
/// Fold an observed failure-event history into the ten retry counters and
/// the budget verdict.
///
/// `now` is consulted only for the final poison-TTL check; the window
/// reset and the backoff deadline use the events' own timestamps. `fleet`
/// is consulted only by the `Transient` events' fleet-exhaust arm.
///
/// The verdict is the disposition as of the end of the history: the
/// verdict produced by the last decision-bearing event, downgraded to
/// [`Verdict::TtlExpire`] if the derivation is poisoned and the TTL has
/// elapsed by `now`.
pub(crate) fn reference_fold(
    history: &[AttemptEvent],
    now: AbsTime,
    budget: &Budget,
    fleet: &FleetView,
) -> (Counters, Verdict) {
    fold_events(Counters::default(), history, now, budget, fleet)
}

/// The fold body shared by [`reference_fold`] (which always starts from
/// the default counters) and [`decide`] (which may start from the
/// transitional legacy seed): apply every event in order, then downgrade
/// a stale poison to [`Verdict::TtlExpire`].
fn fold_events(
    initial: Counters,
    history: &[AttemptEvent],
    now: AbsTime,
    budget: &Budget,
    fleet: &FleetView,
) -> (Counters, Verdict) {
    let mut c = initial;
    let mut verdict = Verdict::Requeue;

    for ev in history {
        verdict = apply(&mut c, ev, budget, fleet);
    }

    // r[impl sched.state.poisoned-ttl]
    // The TTL is a property of (poisoned_at, now), not of the event
    // sequence: `tick_process_expired_poisons` discovers it by scanning,
    // not by receiving an event.
    if matches!(verdict, Verdict::Poison(_))
        && let Some(p) = c.poisoned_at
        && now.saturating_sub(p) > budget.poison_ttl_secs
    {
        verdict = Verdict::TtlExpire;
    }

    (c, verdict)
}

/// Apply one event to the counters and return the verdict it produces.
/// Each arm cites the entry point it transcribes; the `DIVERGENCE` arms
/// deliberately deviate from the code per the invariant map.
fn apply(c: &mut Counters, ev: &AttemptEvent, budget: &Budget, fleet: &FleetView) -> Verdict {
    match ev {
        // ── E1: handle_transient_failure ────────────────────────────
        // Order matters and is the code's: record the failure first
        // (insert + increment), then check the poison threshold over the
        // set that now includes this failure, then check the fleet, then
        // check the per-cycle count cap, and only on the retry arm
        // increment `count` and arm the backoff.
        AttemptEvent::Transient { at, executor } => {
            c.failed_builders.insert(executor.clone());
            c.failure_count += 1;
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                return Verdict::Poison(PoisonReason::Threshold);
            }
            if exhausts_fleet(&c.failed_builders, fleet) {
                c.poisoned_at = Some(*at);
                return Verdict::Poison(PoisonReason::FleetExhausted);
            }
            if c.count < budget.max_retries {
                // backoff is computed from the count BEFORE the
                // increment: the first retry waits base seconds.
                let backoff = budget.backoff_secs(c.count);
                c.count += 1;
                c.backoff_until = Some(at.saturating_add(backoff));
                Verdict::Requeue
            } else {
                // P3 (keep-and-document): under production defaults
                // (`require_distinct_workers = true`, threshold 3,
                // `max_retries` 2, one-shot executors that are always
                // distinct and excluded from placement once failed) the
                // distinct-worker threshold above fires at the same
                // failure as — or before — this per-cycle cap, so this
                // arm is defaults-shadowed. It stays because
                // `sched.retry.transient-budget`'s final clause mandates
                // it and it is live whenever the threshold exceeds
                // `max_retries + 1` or distinct-worker counting is off
                // (single-worker dev deployments).
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::TransientBudget)
            }
        }

        // ── E2: handle_infrastructure_failure ───────────────────────
        // The arm mirrors the handler's own statement order: the exempt
        // block first (increment + its own cap check, with NO early
        // return on the under-cap path), then the I-127 window reset,
        // then the non-exempt cap check and charge.
        AttemptEvent::Infra {
            at,
            executor: _,
            exempt,
            at_cap,
        } => {
            if *exempt {
                // r[impl sched.retry.exempt-infra-cap]
                // Increment-then-check: the cap fires ON the Nth exempt
                // attempt (a different fencepost from the non-exempt arm
                // below — divergence A10). The under-cap exempt path
                // does not return here: the as-built handler falls
                // through to the window reset below.
                c.exempt_infra_count += 1;
                if c.exempt_infra_count >= budget.max_exempt_infra_retries {
                    c.poisoned_at = Some(*at);
                    return Verdict::Poison(PoisonReason::ExemptInfraBudget);
                }
            }
            // The I-127 sliding window: an infra failure more than
            // `infra_retry_window_secs` after the previous counted one
            // is a fresh incident — reset the counter before the cap
            // check. The guard is the event's own floor outcome only:
            // at-cap resource exhaustion is deterministic, so the
            // sparse-vs-burst forgiveness does not apply to it. It is
            // NOT gated on the exemption — an under-cap exempt failure
            // (CONCURRENT_PUTPATH or floor-promoted) arriving past the
            // window also zeroes `infra_count`, exactly as the as-built
            // handler does (its exempt block falls through to the
            // reset). The exempt event itself still charges only
            // `exempt_infra_count` and does not move the window anchor.
            if !*at_cap
                && let Some(last) = c.last_infra_failure_at
                && at.saturating_sub(last) > budget.infra_retry_window_secs
            {
                c.infra_count = 0;
            }
            if *exempt {
                return Verdict::Requeue;
            }
            // Check-then-increment: the cap fires on failure N+1.
            if c.infra_count >= budget.max_infra_retries {
                c.poisoned_at = Some(*at);
                return Verdict::Poison(PoisonReason::InfraBudget);
            }
            c.infra_count += 1;
            c.last_infra_failure_at = Some(*at);
            // No `failed_builders` insert, no `count` increment, no
            // backoff: infra failures are worker-local, not the build's
            // fault, and the requeue is immediate.
            Verdict::Requeue
        }

        // ── E3: handle_permanent_failure ────────────────────────────
        AttemptEvent::Permanent { at, executor } => {
            c.poisoned_at = Some(*at);
            // Diagnostics-only insert (I-209): `failed_builders` gates
            // nothing on the permanent path, but it IS a counter
            // mutation the fold must reproduce. `failure_count` is NOT
            // incremented here (asymmetry A6: the recovery projection's
            // `failed_builders.len()` will count this executor anyway).
            c.failed_builders.insert(executor.clone());
            Verdict::Poison(PoisonReason::Permanent)
        }

        // ── E4: handle_timeout_failure ──────────────────────────────
        // r[impl sched.timeout.promote-on-exceed+3]
        AttemptEvent::WorkerTimeout { at: _, executor: _ } => {
            if c.timeout_count < budget.max_timeout_retries {
                c.timeout_count += 1;
                // No backoff: the next dispatch's doubled deadline is
                // the backoff. No `failed_builders` insert: the same
                // worker with a longer deadline would succeed.
                Verdict::Requeue
            } else {
                // Terminal Cancelled, NOT Poisoned: immediately
                // retriable on explicit resubmit, no 24 h lockout.
                // `poisoned_at` is not set.
                Verdict::Cancel
            }
        }

        // ── E5: reassign_derivations ────────────────────────────────
        // A bare disconnect charges nothing — the controller's follow-up
        // report is authoritative on whether the death was a sizing
        // signal, and a worker that genuinely failed sends a
        // CompletionReport before disconnecting. Only the existing
        // poison state is re-checked (3 prior recorded failures + this
        // disconnect → poison instead of a 4th dispatch). Note the
        // fleet-exhaust check is NOT re-run here (only E1 and E9 run
        // it).
        AttemptEvent::Disconnect { at, executor: _ } => {
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::Threshold)
            } else {
                Verdict::Requeue
            }
        }

        // ── E6: handle_executor_termination ─────────────────────────
        AttemptEvent::ControllerTermination {
            at,
            executor: _,
            promoted,
            at_cap,
        } => {
            if *at_cap {
                // The pod died at the resource ceiling: there is no
                // worker report, so this path owns the cap check and the
                // increment. Check-then-increment, same fencepost as E2.
                if c.infra_count >= budget.max_infra_retries {
                    c.poisoned_at = Some(*at);
                    return Verdict::Poison(PoisonReason::InfraBudget);
                }
                c.infra_count += 1;
                // DIVERGENCE D2: the as-built E6 does not stamp
                // `last_infra_failure_at`; the fold stamps it on every
                // `infra_count` increment (the field's documented
                // meaning — the window measures the gap since the last
                // *counted* infra failure, whichever channel counted
                // it). `CountersRefineHistory` is expected to differ
                // from the live state here.
                c.last_infra_failure_at = Some(*at);
                return Verdict::Requeue;
            }
            if *promoted {
                // DIVERGENCE D3 / CONTRADICTION C3: the as-built E6
                // charges nothing for a promoted controller-reported
                // OOM; `sched.retry.exempt-infra-cap` defines an exempt
                // attempt as "CONCURRENT_PUTPATH or
                // `floor_outcome.promoted`" and mandates that every
                // exempt attempt charges `exempt_infra_count`. The fold
                // does what the rule mandates; the code's no-charge is
                // the deviation Phase 1 must disposition.
                c.exempt_infra_count += 1;
                if c.exempt_infra_count >= budget.max_exempt_infra_retries {
                    c.poisoned_at = Some(*at);
                    return Verdict::Poison(PoisonReason::ExemptInfraBudget);
                }
                return Verdict::Requeue;
            }
            // Neither promoted nor at-cap: a cold-start termination with
            // no intent to double from, or a dimension that cannot grow.
            // The code charges nothing and so does the fold.
            Verdict::Requeue
        }

        // ── E7: handle_deadline_exceeded ────────────────────────────
        AttemptEvent::ControllerDeadlineExceeded { at: _, executor: _ } => {
            if c.timeout_count >= budget.max_timeout_retries {
                // DIVERGENCE D1: the as-built E7 calls
                // `poison_and_cascade` here (24 h TTL, bounded
                // resubmit); E4 produces terminal `Cancelled` for the
                // same exhausted budget, and
                // `sched.timeout.promote-on-exceed+3` names `Cancelled`
                // as the timeout-cap terminal state. The two reports
                // describe the same physical deadline overrun and which
                // arrives first is a race, so a channel-invariant fold
                // must pick one: the fold produces the spec-mandated
                // `Cancel`, and E7's `Poisoned` is the deviation.
                // `VerdictIsChannelInvariant` is expected to falsify on
                // exactly this history.
                return Verdict::Cancel;
            }
            // Check-then-increment: the same 4-retries-then-terminal
            // fencepost as E4's increment-only-on-the-retry-arm.
            c.timeout_count += 1;
            Verdict::Requeue
        }

        // ── E8: tick_process_backstop_timeouts ──────────────────────
        // r[impl sched.backstop.timeout+3]
        // The no-report path: completion.rs will never account this
        // attempt, so the backstop records it (insert + increment) and
        // then delegates to E5's reassign, whose threshold re-check now
        // sees the increment. The PG mirror of this insert is missing
        // (divergence D4) — that is a property of the durable view, not
        // of these in-memory counters.
        AttemptEvent::BackstopTimeout { at, executor } => {
            c.failed_builders.insert(executor.clone());
            c.failure_count += 1;
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::Threshold)
            } else {
                Verdict::Requeue
            }
        }

        // ── The establishment sweep (C2, Phase 1b T-1b.11) ──────────
        // r[impl sched.retry.per-executor-budget+2]
        // A released execution whose classifying report never arrived,
        // established by the correlation-TTL sweep (or recorded by the
        // backstop, which has its own arm above): charges the
        // threshold/exclusion budget — `failed_builders[executor]` +
        // `failure_count`, nothing else (decision P1) — and re-checks
        // the threshold, so a derivation that deterministically kills
        // its worker with no report is bounded by the same budget the
        // per-executor-budget rule's "executor disconnect DOES count"
        // clause names. The not-yet-established `Disconnect` event
        // stays uncharged (the classification window must stay open
        // for the controller's report).
        AttemptEvent::EstablishedCrash { at, executor } => {
            c.failed_builders.insert(executor.clone());
            c.failure_count += 1;
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::Threshold)
            } else {
                Verdict::Requeue
            }
        }

        // ── assign_to_worker ────────────────────────────────────────
        AttemptEvent::Dispatched { .. } => {
            c.backoff_until = None;
            Verdict::Requeue
        }

        // ── dag::merge resubmit reset ───────────────────────────────
        // r[impl sched.merge.poisoned-resubmit-bounded+3]
        // A fresh `DerivationState` is constructed (all counters at
        // their defaults, including `backoff_until`) and
        // `resubmit_cycles` is carried over and incremented — the reset
        // itself is the cycle event.
        AttemptEvent::ResubmitReset { .. } => {
            let cycles = c.resubmit_cycles;
            *c = Counters::default();
            c.resubmit_cycles = cycles + 1;
            Verdict::Requeue
        }

        // ── cache-hit clear ─────────────────────────────────────────
        AttemptEvent::CacheHitClear { .. } => {
            c.clear_for_cache_hit();
            Verdict::Requeue
        }

        // ── admin ClearPoison / TTL expiry ──────────────────────────
        AttemptEvent::PoisonCleared { .. } => {
            *c = Counters::default();
            Verdict::Requeue
        }
    }
}

// ---------------------------------------------------------------------------
// Phase-1b decision surface: decide() / classify() / placeable()
//
// The design's frozen §5a-2 contract, layered on the fold above. The nine
// entry points become callers of these three functions as they collapse
// (T-1b.2 onward); the fold core (`reference_fold` / `apply`) stays the
// executable spec and is not changed by the collapse.
// ---------------------------------------------------------------------------

/// The decision-surface output for one appending-transaction read: the
/// budget verdict plus the derived views the call sites consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The budget verdict as of the end of the history.
    pub verdict: Verdict,
    /// The per-executor exclusion set (the fold's `failed_builders`) in
    /// the actor's identifier vocabulary. E1's fleet-exhaust arm and the
    /// E9 dispatch backstop intersect it with the live eligible fleet via
    /// [`placeable`]; `hard_filter` consumes the same set through the
    /// fold-refreshed cached view (`RetryState::failed_builders`).
    pub exclusion: BTreeSet<ExecutorId>,
    /// The deterministic backoff deadline (no jitter — the dispatch site
    /// applies the production jitter exactly as today).
    pub backoff_until: Option<AbsTime>,
    /// The full fold-derived counter view.
    pub counters: Counters,
}

/// Phase-1b decision function: fold a derivation's attempt-ledger suffix
/// into the budget verdict and the derived counter/exclusion views.
///
/// `history` is the post-reset suffix in ledger order (what
/// `load_attempt_suffix` returns, or the in-memory mirror of it),
/// INCLUDING the row the calling site just appended — the verdict is the
/// disposition produced by the last decision-bearing event. `now` is
/// epoch seconds (the same clock as `AttemptRecord::occurred_at_epoch_secs`)
/// and is consulted only for the poison-TTL downgrade.
///
/// The fleet-exhaust arm is deliberately NOT part of this fold: the
/// eligible fleet is not history, so the in-history check is evaluated
/// against an empty fleet (never exhausted) and the call sites consume
/// [`Decision::exclusion`] through [`placeable`] instead.
///
/// `legacy_seed` is the **transitional** mixed-era input (decision P5 /
/// design amendment A1, removed in Phase 2 with the mirror-column drop):
/// when the derivation's `derivations.{retry_count, failed_builders,
/// resubmit_cycles}` columns are non-empty and the suffix contains no
/// reset row, the fold is floored by the legacy projection so a failure
/// history that spans the 066 deployment keeps every counter that
/// survives failover today. The floor semantics are union for
/// `failed_builders`, max for `count` and `resubmit_cycles`, and
/// `failure_count` seeded from the legacy set's size — never below what
/// either era supports, and the set/threshold view cannot double-count
/// because set inserts are idempotent. (`count`'s floor is applied after
/// the fold, so the per-cycle transient cap check inside the fold sees
/// only the post-066 rows during the transition; the distinct-worker
/// threshold — the production-default bound — sees the merged set.) A
/// suffix that begins with a reset row ignores the seed; an empty suffix
/// degenerates to the pure legacy projection. This one function is the
/// shared merge point for every fold input construction: the appending
/// transactions read the columns on their own connection
/// (`load_retry_seed_in_tx`), while recovery's retry-view rebuild and
/// the dispatch-time fleet-exhaust check pass the floor carried on the
/// node (`DerivationState::legacy_retry_floor`), so all of them apply
/// exactly the same P5 semantics (`sched.retry.recovery-projection+2`).
//
// ── Kani contracts ───────────────────────────────────────────────────
// No requires clause: the contract holds over the full input domain.
// Counter arithmetic cannot overflow at any reachable suffix length
// (every per-event charge is +1 onto a u32 and the clock arithmetic is
// saturating), so the harness bound on history length is a solver
// budget, not a soundness precondition. The four ensures clauses are,
// in order: the verdict partition is consistent with the counters it
// was computed from (each terminal verdict names a budget that really
// is at its bound, the TTL downgrade only fires on a stamped expired
// poison, and the fleet-exhaust reason is unreachable from decide() —
// placement is placeable()'s job); a Requeue verdict never exceeds a
// budget cap (the per-cycle/infra/timeout caps hold over every history,
// the exempt cap over every history whose last event is exempt-charging
// — the global form additionally needs the writer discipline that
// poisoned nodes get no further attempt rows, which is upstream of the
// fold); the exclusion set contains the executor of every charged
// threshold attempt after the last reset row plus everything the legacy
// seed holds; and the legacy-seed merge never drops a counter below
// what the frozen mirror columns support (decision P5 / design
// amendment A1). Verified by `check_decide_contract` in
// `#[cfg(kani)] mod proofs`; the two-call properties (determinism,
// seed-vs-unseeded monotonicity, the reset-row seed bypass) are the
// `check_decide_deterministic` and `check_legacy_seed_merge_monotone`
// harnesses.
#[cfg_attr(kani, kani::ensures(|d: &Decision| {
    match d.verdict {
        Verdict::Requeue => true,
        Verdict::Poison(PoisonReason::Threshold) => {
            d.counters.poisoned_at.is_some() && d.counters.poison_threshold_reached(budget)
        }
        Verdict::Poison(PoisonReason::TransientBudget) => {
            d.counters.poisoned_at.is_some() && d.counters.count >= budget.max_retries
        }
        Verdict::Poison(PoisonReason::InfraBudget) => {
            d.counters.poisoned_at.is_some() && d.counters.infra_count >= budget.max_infra_retries
        }
        Verdict::Poison(PoisonReason::ExemptInfraBudget) => {
            d.counters.poisoned_at.is_some()
                && d.counters.exempt_infra_count >= budget.max_exempt_infra_retries
        }
        Verdict::Poison(PoisonReason::Permanent) => d.counters.poisoned_at.is_some(),
        Verdict::Poison(PoisonReason::FleetExhausted) => false,
        Verdict::Cancel => d.counters.timeout_count >= budget.max_timeout_retries,
        Verdict::TtlExpire => matches!(
            d.counters.poisoned_at,
            Some(p) if now.saturating_sub(p) > budget.poison_ttl_secs
        ),
    }
}))]
#[cfg_attr(kani, kani::ensures(|d: &Decision| {
    let has_reset = history
        .iter()
        .any(|r| r.event_kind == AttemptEventKind::Reset);
    let seeded_count_floor = match legacy_seed {
        Some(s) if !has_reset && !s.is_empty() => s.retry_count,
        _ => 0,
    };
    let last_is_exempt_charge = history.last().is_some_and(|r| {
        r.event_kind == AttemptEventKind::Attempt
            && ((r.reporting_party == ReportingParty::Worker
                && r.outcome_class == OutcomeClass::ExemptInfra)
                || (r.reporting_party != ReportingParty::Worker
                    && !r.floor_at_cap
                    && (r.outcome_class == OutcomeClass::ExemptInfra
                        || (r.outcome_class == OutcomeClass::Infra && r.floor_promoted))))
    });
    d.counters.count <= budget.max_retries.max(seeded_count_floor)
        && d.counters.infra_count <= budget.max_infra_retries
        && d.counters.timeout_count <= budget.max_timeout_retries
        && (!matches!(d.verdict, Verdict::Requeue)
            || !last_is_exempt_charge
            || d.counters.exempt_infra_count < budget.max_exempt_infra_retries)
}))]
#[cfg_attr(kani, kani::ensures(|d: &Decision| {
    let last_reset = history
        .iter()
        .rposition(|r| r.event_kind == AttemptEventKind::Reset);
    let start = last_reset.map_or(0, |i| i + 1);
    let charged_ok = history[start..].iter().all(|r| {
        let charges_threshold = r.event_kind == AttemptEventKind::Attempt
            && matches!(
                r.outcome_class,
                OutcomeClass::Transient
                    | OutcomeClass::Permanent
                    | OutcomeClass::Backstop
                    | OutcomeClass::ExecutorCrash
            );
        !charges_threshold
            || r.executor_id
                .as_ref()
                .is_none_or(|e| d.exclusion.contains(e))
    });
    let seed_ok = match legacy_seed {
        Some(s) if last_reset.is_none() && !s.is_empty() => s
            .failed_builders
            .iter()
            .all(|w| d.exclusion.contains(&ExecutorId::from(w.as_str()))),
        _ => true,
    };
    charged_ok && seed_ok
}))]
#[cfg_attr(kani, kani::ensures(|d: &Decision| {
    let has_reset = history
        .iter()
        .any(|r| r.event_kind == AttemptEventKind::Reset);
    match legacy_seed {
        Some(s) if !has_reset && !s.is_empty() => {
            d.counters.count >= s.retry_count
                && d.counters.resubmit_cycles >= s.resubmit_cycles
                && d.counters.failure_count >= s.failed_builders.len() as u32
                && s.failed_builders
                    .iter()
                    .all(|w| d.counters.failed_builders.contains(w))
        }
        _ => true,
    }
}))]
pub(crate) fn decide(
    history: &[AttemptRecord],
    budget: &Budget,
    now: AbsTime,
    legacy_seed: Option<&PersistedRetryColumns>,
) -> Decision {
    let has_reset = history
        .iter()
        .any(|r| r.event_kind == AttemptEventKind::Reset);
    let seed = legacy_seed.filter(|s| !has_reset && !s.is_empty());

    let mut initial = Counters::default();
    if let Some(s) = seed {
        // Set-shaped state seeds up front so the distinct-worker
        // threshold / exclusion checks inside the fold see both eras
        // (idempotent inserts make double-counting impossible). The
        // flat counters (`count`, `failure_count`) are deliberately NOT
        // seeded: the still-active legacy writers mirror the current
        // era into the columns too, so a pre-fold seed would count the
        // suffix's own rows twice; their floors are applied after the
        // fold instead (max / merged-set size — the P5 floor
        // semantics). No event in a reset-free suffix touches
        // `resubmit_cycles`, so seeding it equals the max() floor.
        initial.failed_builders = s.failed_builders.clone();
        initial.resubmit_cycles = s.resubmit_cycles;
        initial.poisoned_at = s.poisoned_at;
    }
    // A suffix that starts at a resubmit-reset row carries the new cycle
    // index on the row itself; seed the pre-fold counter so the reset
    // arm's `prior + 1` reproduces it (the loader cuts the suffix at the
    // most recent reset, so prior cycles are not otherwise visible).
    if let Some(first) = history.first()
        && first.event_kind == AttemptEventKind::Reset
        && first.outcome_class == OutcomeClass::ResubmitReset
    {
        initial.resubmit_cycles = u32::try_from(first.resubmit_cycle)
            .unwrap_or(0)
            .saturating_sub(1);
    }

    let events: Vec<AttemptEvent> = history.iter().filter_map(record_to_event).collect();
    let (mut counters, verdict) = fold_events(initial, &events, now, budget, &FleetView::default());

    if let Some(s) = seed {
        // The legacy floors for the flat counters (P5): max, not sum —
        // post-066 rows that the still-active legacy writers also
        // mirrored into the columns must not count twice.
        counters.count = counters.count.max(s.retry_count);
        counters.failure_count = counters
            .failure_count
            .max(counters.failed_builders.len() as u32);
    }

    Decision {
        verdict,
        exclusion: counters
            .failed_builders
            .iter()
            .map(|s| ExecutorId::from(s.clone()))
            .collect(),
        backoff_until: counters.backoff_until,
        counters,
    }
}

/// Map one ledger record onto the fold's event alphabet. Returns `None`
/// for rows that are deliberately no-ops for the fold: the per-dependent
/// `cascade` rows (the trigger's own poison row carries the charge) and
/// the E9 `fleet_exhaust` verdict marker (the placement verdict is
/// re-derived from the exclusion set and the live fleet by [`placeable`],
/// never folded from history).
fn record_to_event(record: &AttemptRecord) -> Option<AttemptEvent> {
    let at = record.occurred_at_epoch_secs as AbsTime;
    let executor = record
        .executor_id
        .as_ref()
        .map(|e| e.as_str().to_string())
        .unwrap_or_default();
    if record.event_kind == AttemptEventKind::Reset {
        return match record.outcome_class {
            OutcomeClass::ResubmitReset => Some(AttemptEvent::ResubmitReset { at }),
            OutcomeClass::CacheHitClear => Some(AttemptEvent::CacheHitClear { at }),
            OutcomeClass::PoisonCleared => Some(AttemptEvent::PoisonCleared { at }),
            // A reset row never carries an attempt class (writer
            // discipline + the migration's CHECK); fold a malformed one
            // as a no-op rather than guess.
            _ => None,
        };
    }
    let worker_reported = record.reporting_party == ReportingParty::Worker;
    match record.outcome_class {
        OutcomeClass::Transient => Some(AttemptEvent::Transient { at, executor }),
        OutcomeClass::Infra | OutcomeClass::ExemptInfra => {
            let exempt = record.outcome_class == OutcomeClass::ExemptInfra;
            if worker_reported {
                // E2 — the worker-reported arm, including its exempt
                // fall-through to the stale-window reset.
                Some(AttemptEvent::Infra {
                    at,
                    executor,
                    exempt,
                    at_cap: record.floor_at_cap,
                })
            } else {
                // E6 — a controller-classified attempt (the two-installment
                // fill, or the race-ahead append). The exemption rides on
                // the class itself; `at_cap` on the stored floor flag.
                //
                // Accepted limitation for T-1b.1–T-1b.6: the 1a second
                // installment fills only `termination_reason` +
                // `outcome_class`, so an at-cap controller termination's
                // row still reads `floor_at_cap = false` and folds as the
                // charges-nothing arm here. The as-built E6 site keeps
                // enforcing the at-cap infra cap from RAM until its own
                // collapse (T-1b.9), which owns making the installment
                // carry the floor outcome.
                Some(AttemptEvent::ControllerTermination {
                    at,
                    executor,
                    promoted: exempt || record.floor_promoted,
                    at_cap: record.floor_at_cap,
                })
            }
        }
        OutcomeClass::Timeout => {
            if worker_reported {
                Some(AttemptEvent::WorkerTimeout { at, executor })
            } else {
                Some(AttemptEvent::ControllerDeadlineExceeded { at, executor })
            }
        }
        OutcomeClass::Permanent => Some(AttemptEvent::Permanent { at, executor }),
        OutcomeClass::Backstop => Some(AttemptEvent::BackstopTimeout { at, executor }),
        // First-installment disconnect rows: classification not yet
        // established; charges nothing, re-checks the threshold (E5).
        OutcomeClass::Disconnected => Some(AttemptEvent::Disconnect { at, executor }),
        // C2 (T-1b.11): an established unreported executor crash (the
        // TTL sweep filled `termination_reason='unreported'`) charges
        // the threshold/exclusion budget.
        OutcomeClass::ExecutorCrash => Some(AttemptEvent::EstablishedCrash { at, executor }),
        OutcomeClass::Cascade | OutcomeClass::FleetExhaust => None,
        // Reset classes only ever ride on `event_kind = 'reset'` rows
        // (handled above); an attempt-kind row carrying one is malformed
        // — fold it as a no-op rather than guess.
        OutcomeClass::ResubmitReset | OutcomeClass::CacheHitClear | OutcomeClass::PoisonCleared => {
            None
        }
    }
}

/// The floor-bump outcome as [`classify`] consumes it — a leaf-local
/// mirror of the actor's `FloorOutcome` so this module keeps no actor
/// dependency. `promoted` and `at_cap` are mutually exclusive; both are
/// false for non-resource events and for the cold-start no-intent case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FloorOutcomeView {
    /// The floor doubled — the attempt is a sizing signal, exempt from
    /// the non-exempt infra cap.
    pub promoted: bool,
    /// The relevant dimension was already at its ceiling.
    pub at_cap: bool,
}

/// One observed failure trigger, as the entry point sees it at append
/// time — the input vocabulary of [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedFailure<'a> {
    /// E1 — worker `CompletionReport{TransientFailure}` or `Unspecified`.
    WorkerTransient,
    /// E2 — worker `CompletionReport{InfrastructureFailure}`; the error
    /// message drives the CONCURRENT_PUTPATH half of the exemption.
    WorkerInfra { error_msg: &'a str },
    /// E3 — one of the seven permanent failure statuses.
    WorkerPermanent,
    /// E4 — worker `CompletionReport{TimedOut}`.
    WorkerTimeout,
    /// E5 — stream disconnect / heartbeat timeout / force-drain released
    /// the execution; classification not yet established.
    Disconnect,
    /// E6 — controller `ReportExecutorTermination{OomKilled,
    /// EvictedDiskPressure}`.
    ControllerResourceTermination,
    /// E7 — controller `ReportExecutorTermination{DeadlineExceeded}`.
    ControllerDeadlineExceeded,
    /// E8 — the scheduler-side backstop timer fired for a Running build
    /// with no report.
    BackstopTimeout,
    /// The correlation-TTL sweep (or backstop) established a disconnect
    /// whose classifying report never arrived.
    UnreportedCrash,
}

// r[impl sched.retry.exempt-infra-cap]
/// The third total function of the decision surface: classify one
/// observed failure event into the ledger's outcome-class alphabet,
/// consuming the floor outcome at append time so [`decide`] never sees
/// the floor (G6's bug class becomes a classification bug with this
/// single checked contract).
///
/// The exemption predicate is exactly E2's as-built `exempt_from_cap`
/// (`floor_outcome.promoted || CONCURRENT_PUTPATH`), extended to the
/// controller channel per `sched.retry.exempt-infra-cap`'s "every exempt
/// attempt" (divergence D3's adjudicated side; the charge becomes
/// decision-visible as the sites collapse). A transient failure never
/// consults the floor (P4).
//
// ── Kani contract ────────────────────────────────────────────────────
// The single ensures clause is the classification partition stated as
// an iff per observed-event variant: each trigger maps to exactly the
// ledger class its entry point appends, the exemption predicate is
// precisely `floor.promoted || CONCURRENT_PUTPATH` on the worker
// channel and `floor.promoted` on the controller channel (the
// `sched.retry.exempt-infra-cap` definition of an exempt attempt, on
// both channels — D3's adjudicated side), a transient failure never
// classifies as exempt regardless of the floor outcome (P4), and no
// reset/cascade/fleet class is ever produced for an observed failure.
// Verified over the full type domain (with representative error
// messages) by `check_classify_contract` in `#[cfg(kani)] mod proofs`.
#[cfg_attr(kani, kani::ensures(|c: &OutcomeClass| {
    match event {
        ObservedFailure::WorkerTransient => *c == OutcomeClass::Transient,
        ObservedFailure::WorkerInfra { error_msg } => {
            if floor.promoted || error_msg.contains(rio_proto::CONCURRENT_PUTPATH_MSG) {
                *c == OutcomeClass::ExemptInfra
            } else {
                *c == OutcomeClass::Infra
            }
        }
        ObservedFailure::WorkerPermanent => *c == OutcomeClass::Permanent,
        ObservedFailure::WorkerTimeout => *c == OutcomeClass::Timeout,
        ObservedFailure::Disconnect => *c == OutcomeClass::Disconnected,
        ObservedFailure::ControllerResourceTermination => {
            if floor.promoted {
                *c == OutcomeClass::ExemptInfra
            } else {
                *c == OutcomeClass::Infra
            }
        }
        ObservedFailure::ControllerDeadlineExceeded => *c == OutcomeClass::Timeout,
        ObservedFailure::BackstopTimeout => *c == OutcomeClass::Backstop,
        ObservedFailure::UnreportedCrash => *c == OutcomeClass::ExecutorCrash,
    }
}))]
pub(crate) fn classify(event: &ObservedFailure<'_>, floor: FloorOutcomeView) -> OutcomeClass {
    match event {
        ObservedFailure::WorkerTransient => OutcomeClass::Transient,
        ObservedFailure::WorkerInfra { error_msg } => {
            if floor.promoted || error_msg.contains(rio_proto::CONCURRENT_PUTPATH_MSG) {
                OutcomeClass::ExemptInfra
            } else {
                OutcomeClass::Infra
            }
        }
        ObservedFailure::WorkerPermanent => OutcomeClass::Permanent,
        ObservedFailure::WorkerTimeout => OutcomeClass::Timeout,
        ObservedFailure::Disconnect => OutcomeClass::Disconnected,
        ObservedFailure::ControllerResourceTermination => {
            if floor.promoted {
                OutcomeClass::ExemptInfra
            } else {
                OutcomeClass::Infra
            }
        }
        ObservedFailure::ControllerDeadlineExceeded => OutcomeClass::Timeout,
        ObservedFailure::BackstopTimeout => OutcomeClass::Backstop,
        ObservedFailure::UnreportedCrash => OutcomeClass::ExecutorCrash,
    }
}

/// The placement verdict for a derivation given its exclusion set and
/// the live eligible fleet — [`exhausts_fleet`]'s answer plus the
/// "is there anyone left to take it" discrimination the dispatch site
/// needs. Pure; the operator-facing exhaustion observability (warn! +
/// metric) stays at the call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// At least one eligible worker is not in the exclusion set.
    Placeable,
    /// The eligible fleet is non-empty and every member has already
    /// failed this derivation — the fleet-exhaust poison arm (E1/E9).
    FleetExhausted,
    /// No statically-eligible, non-draining worker is registered at all:
    /// defer, never poison (the empty-fleet clause of
    /// `sched.dispatch.fleet-exhaust+3` — an empty pool is a
    /// provisioning transient).
    NoEligibleWorkers,
}

// r[impl sched.dispatch.fleet-exhaust+3]
/// The fleet-exhaust / placement predicate consumed by E1's fleet arm
/// and the E9 dispatch backstop: intersect [`Decision::exclusion`] with
/// the caller's snapshot of the statically-eligible, non-draining,
/// registered fleet. Mirrors [`exhausts_fleet`] (and the as-built
/// `failed_builders_exhausts_fleet`): an empty exclusion set or an empty
/// eligible fleet never reads as exhausted.
//
// ── Kani contract ────────────────────────────────────────────────────
// The ensures clause is the placement partition stated as an iff per
// variant: an empty eligible fleet always defers (never poisons — the
// empty-fleet clause of `sched.dispatch.fleet-exhaust+3`), exhaustion
// requires a non-empty fleet every member of which is excluded AND a
// non-empty exclusion set, and anything else is placeable. Verified by
// `check_placeable_contract` in `#[cfg(kani)] mod proofs`.
#[cfg_attr(kani, kani::ensures(|p: &Placement| {
    match p {
        Placement::NoEligibleWorkers => eligible.is_empty(),
        Placement::FleetExhausted => {
            !eligible.is_empty()
                && !excluded.is_empty()
                && eligible.iter().all(|w| excluded.contains(w))
        }
        Placement::Placeable => {
            !eligible.is_empty()
                && (excluded.is_empty() || eligible.iter().any(|w| !excluded.contains(w)))
        }
    }
}))]
pub(crate) fn placeable(
    excluded: &BTreeSet<ExecutorId>,
    eligible: &BTreeSet<ExecutorId>,
) -> Placement {
    if eligible.is_empty() {
        return Placement::NoEligibleWorkers;
    }
    if !excluded.is_empty() && eligible.iter().all(|w| excluded.contains(w)) {
        Placement::FleetExhausted
    } else {
        Placement::Placeable
    }
}

// r[verify sched.retry.transient-budget]
// r[verify sched.retry.attempts-bounded+2]
// r[verify sched.retry.counters-refine-history+2]
#[cfg(test)]
mod tests {
    use super::*;

    fn t(at: AbsTime, ex: &str) -> AttemptEvent {
        AttemptEvent::Transient {
            at,
            executor: ex.into(),
        }
    }

    fn infra(at: AbsTime, ex: &str, exempt: bool, at_cap: bool) -> AttemptEvent {
        AttemptEvent::Infra {
            at,
            executor: ex.into(),
            exempt,
            at_cap,
        }
    }

    fn fleet(workers: &[&str]) -> FleetView {
        FleetView {
            eligible: workers.iter().map(|w| w.to_string()).collect(),
        }
    }

    fn fold(history: &[AttemptEvent], now: AbsTime) -> (Counters, Verdict) {
        reference_fold(history, now, &Budget::default(), &FleetView::default())
    }

    /// Empty history: everything zero, eligible for dispatch.
    #[test]
    fn empty_history_is_requeue_with_default_counters() {
        let (c, v) = fold(&[], 0);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);
    }

    /// The transient budget: with `max_retries = 2` and same-worker
    /// failures (so the distinct-worker threshold of 3 never trips), the
    /// first two failures requeue with an exponential backoff and the
    /// third poisons via the count cap. Charges `count`,
    /// `failure_count`, `failed_builders`, and `backoff_until`.
    #[test]
    fn transient_budget_two_retries_then_poison() {
        let h = [t(100, "w1"), t(200, "w1"), t(300, "w1")];

        let (c, v) = fold(&h[..1], 100);
        assert_eq!(c.count, 1);
        assert_eq!(c.failure_count, 1);
        assert_eq!(c.failed_builders.len(), 1);
        // backoff(0) = base = 5 s, computed from the pre-increment count.
        assert_eq!(c.backoff_until, Some(105));
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h[..2], 200);
        assert_eq!(c.count, 2);
        assert_eq!(c.failure_count, 2);
        assert_eq!(c.failed_builders.len(), 1, "same worker dedups");
        // backoff(1) = 5 * 2 = 10 s.
        assert_eq!(c.backoff_until, Some(210));
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 300);
        assert_eq!(c.count, 2, "the poisoning failure does not increment");
        assert_eq!(c.failure_count, 3);
        assert_eq!(c.poisoned_at, Some(300));
        assert_eq!(v, Verdict::Poison(PoisonReason::TransientBudget));
    }

    /// The distinct-worker poison threshold fires on the third distinct
    /// executor, before the count cap gets a say, and a successful
    /// dispatch between failures clears the backoff defer.
    #[test]
    fn distinct_worker_threshold_poisons_on_third_executor() {
        let h = [
            t(100, "w1"),
            AttemptEvent::Dispatched {
                at: 110,
                executor: "w2".into(),
            },
            t(200, "w2"),
            t(300, "w3"),
        ];
        let (c, v) = fold(&h[..2], 110);
        assert_eq!(c.backoff_until, None, "dispatch clears the defer");
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 300);
        assert_eq!(c.failed_builders.len(), 3);
        assert_eq!(c.failure_count, 3);
        assert_eq!(v, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(c.poisoned_at, Some(300));
    }

    /// The fleet-exhaust arm: two distinct workers fail (below the
    /// threshold of 3) but the eligible fleet IS those two workers, so
    /// the second failure poisons. An empty fleet never exhausts.
    #[test]
    fn fleet_exhaust_poisons_below_the_threshold() {
        let h = [t(100, "w1"), t(200, "w2")];
        let two = fleet(&["w1", "w2"]);
        let (c, v) = reference_fold(&h, 200, &Budget::default(), &two);
        assert_eq!(c.failed_builders.len(), 2);
        assert_eq!(v, Verdict::Poison(PoisonReason::FleetExhausted));

        // The same history against a fleet with a fresh worker requeues.
        let three = fleet(&["w1", "w2", "w3"]);
        let (_, v) = reference_fold(&h, 200, &Budget::default(), &three);
        assert_eq!(v, Verdict::Requeue);

        // And against an empty fleet (no pool connected) it requeues.
        let (_, v) = reference_fold(&h, 200, &Budget::default(), &FleetView::default());
        assert_eq!(v, Verdict::Requeue);
    }

    /// The non-exempt infra budget: check-then-increment, so
    /// `max_infra_retries = 10` means ten requeues and a poison on the
    /// eleventh failure. Charges `infra_count` and the window anchor and
    /// nothing else.
    #[test]
    fn infra_budget_ten_requeues_then_poison_on_the_eleventh() {
        let mut h: Vec<AttemptEvent> = Vec::new();
        for i in 0..10 {
            h.push(infra(100 + i, "w1", false, false));
        }
        let (c, v) = fold(&h, 200);
        assert_eq!(c.infra_count, 10);
        assert_eq!(c.last_infra_failure_at, Some(109));
        assert_eq!(c.count, 0, "infra does not eat the transient budget");
        assert_eq!(c.failed_builders.len(), 0, "infra never joins the set");
        assert_eq!(c.backoff_until, None, "infra requeues immediately");
        assert_eq!(v, Verdict::Requeue);

        h.push(infra(110, "w1", false, false));
        let (c, v) = fold(&h, 200);
        assert_eq!(
            c.infra_count, 10,
            "the poisoning failure does not increment"
        );
        assert_eq!(v, Verdict::Poison(PoisonReason::InfraBudget));
    }

    /// The 300 s sliding window: two infra failures, then a third more
    /// than the window after the second — the counter resets to zero
    /// before charging, so the third failure leaves `infra_count = 1`. A
    /// fourth within the window accumulates normally. An at-cap failure
    /// is never forgiven by the window.
    #[test]
    fn infra_window_reset_forgives_sparse_failures() {
        let h = [
            infra(100, "w1", false, false),
            infra(150, "w1", false, false),
            // 301 s after the second — strictly greater than the window.
            infra(452, "w1", false, false),
            infra(500, "w1", false, false),
        ];
        let (c, _) = fold(&h[..2], 150);
        assert_eq!(c.infra_count, 2);

        let (c, _) = fold(&h[..3], 452);
        assert_eq!(c.infra_count, 1, "window elapsed: reset then charge");
        assert_eq!(c.last_infra_failure_at, Some(452));

        let (c, _) = fold(&h, 500);
        assert_eq!(c.infra_count, 2, "within the window: accumulate");

        // At-cap failures are exempt from the forgiveness: the same
        // sparse spacing keeps accumulating.
        let h_cap = [
            infra(100, "w1", false, true),
            infra(452, "w1", false, true),
            infra(900, "w1", false, true),
        ];
        let (c, _) = fold(&h_cap, 900);
        assert_eq!(c.infra_count, 3, "at-cap failures never reset");
    }

    /// The as-built E2 fall-through: the I-127 stale-window reset is
    /// gated only on the event's own at-cap outcome, not on the
    /// exemption, so an under-cap exempt infra failure
    /// (CONCURRENT_PUTPATH or floor-promoted) arriving more than the
    /// window after the last counted infra failure also resets
    /// `infra_count` — while itself charging only `exempt_infra_count`
    /// and leaving the window anchor where the last counted failure put
    /// it.
    #[test]
    fn exempt_infra_failure_past_the_window_resets_the_counted_budget() {
        // Counted infra failure (anchor stamped), then an exempt one
        // 350 s later — strictly past the 300 s window.
        let h = [
            infra(100, "w1", false, false),
            infra(450, "w1", true, false),
        ];
        let (c, v) = fold(&h, 450);
        assert_eq!(
            c.infra_count, 0,
            "stale-window reset fires on the exempt event"
        );
        assert_eq!(c.exempt_infra_count, 1);
        assert_eq!(
            c.last_infra_failure_at,
            Some(100),
            "the exempt event does not move the anchor"
        );
        assert_eq!(v, Verdict::Requeue);

        // Within the window the exempt event leaves the counted budget
        // untouched.
        let h = [
            infra(100, "w1", false, false),
            infra(200, "w1", true, false),
        ];
        let (c, _) = fold(&h, 200);
        assert_eq!(c.infra_count, 1, "within the window: no reset");
        assert_eq!(c.exempt_infra_count, 1);
    }

    /// The exempt arm: a CONCURRENT_PUTPATH or floor-promoted infra
    /// failure charges `exempt_infra_count` instead of `infra_count`,
    /// and the exemption's own terminal is increment-then-check — the
    /// cap fires ON the 50th exempt attempt.
    #[test]
    fn exempt_infra_charges_the_exemption_budget_and_terminates() {
        let mut h: Vec<AttemptEvent> = Vec::new();
        for i in 0..49 {
            h.push(infra(100 + i, "w1", true, false));
        }
        let (c, v) = fold(&h, 200);
        assert_eq!(c.exempt_infra_count, 49);
        assert_eq!(c.infra_count, 0, "exempt attempts skip infra_count");
        assert_eq!(
            c.last_infra_failure_at, None,
            "exempt attempts do not move the window anchor"
        );
        assert_eq!(v, Verdict::Requeue);

        h.push(infra(149, "w1", true, false));
        let (c, v) = fold(&h, 200);
        assert_eq!(c.exempt_infra_count, 50);
        assert_eq!(v, Verdict::Poison(PoisonReason::ExemptInfraBudget));
    }

    /// The timeout budget via the worker-reported path: four requeues
    /// (each charging `timeout_count`, no backoff, no exclusion), then
    /// terminal `Cancelled` — not `Poisoned` — on the fifth.
    #[test]
    fn worker_timeout_budget_four_retries_then_cancel() {
        let mk = |at| AttemptEvent::WorkerTimeout {
            at,
            executor: "w1".into(),
        };
        let h: Vec<AttemptEvent> = (0..5).map(|i| mk(100 + i)).collect();
        let (c, v) = fold(&h[..4], 200);
        assert_eq!(c.timeout_count, 4);
        assert_eq!(c.backoff_until, None);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 200);
        assert_eq!(c.timeout_count, 4, "the terminal attempt does not charge");
        assert_eq!(v, Verdict::Cancel);
        assert_eq!(c.poisoned_at, None, "Cancelled is not Poisoned");
    }

    /// DIVERGENCE D1: the same exhausted timeout budget reached via the
    /// controller-reported backstop produces the same `Cancel` verdict
    /// from the fold — the as-built E7 poisons here, and the model's
    /// `VerdictIsChannelInvariant` check is expected to catch that
    /// deviation. The under-cap controller report charges the same
    /// counter as the worker report.
    #[test]
    fn controller_deadline_exceeded_matches_the_worker_timeout_verdict() {
        let wt = |at| AttemptEvent::WorkerTimeout {
            at,
            executor: "w1".into(),
        };
        let cd = |at| AttemptEvent::ControllerDeadlineExceeded {
            at,
            executor: "w1".into(),
        };
        // Four timeouts observed via either channel exhaust the budget;
        // the fifth observation produces Cancel regardless of channel.
        let mixed = [wt(100), cd(200), wt(300), cd(400), cd(500)];
        let (c, v) = fold(&mixed, 500);
        assert_eq!(c.timeout_count, 4);
        assert_eq!(v, Verdict::Cancel, "the fold's adjudicated D1 verdict");

        let all_worker = [wt(100), wt(200), wt(300), wt(400), wt(500)];
        let (c2, v2) = fold(&all_worker, 500);
        assert_eq!(
            (c2.timeout_count, v2),
            (c.timeout_count, v),
            "channel-invariant: same counters, same verdict"
        );
    }

    /// A poison TTL expiry: a permanent failure poisons immediately with
    /// the real `poisoned_at`; once `now` is more than the TTL past it,
    /// the verdict becomes `TtlExpire`. The permanent path records the
    /// executor for diagnostics but never charges `failure_count`.
    #[test]
    fn permanent_failure_poisons_and_the_ttl_expires_it() {
        let h = [AttemptEvent::Permanent {
            at: 1_000,
            executor: "w1".into(),
        }];
        let (c, v) = fold(&h, 1_000);
        assert_eq!(v, Verdict::Poison(PoisonReason::Permanent));
        assert_eq!(c.poisoned_at, Some(1_000));
        assert!(c.failed_builders.contains("w1"), "diagnostics-only insert");
        assert_eq!(c.failure_count, 0, "the permanent path never charges it");

        // Exactly at the TTL boundary the poison holds (strictly-greater
        // comparison, mirroring `duration_since(...) > POISON_TTL`).
        let ttl = Budget::default().poison_ttl_secs;
        let (_, v) = fold(&h, 1_000 + ttl);
        assert_eq!(v, Verdict::Poison(PoisonReason::Permanent));

        let (_, v) = fold(&h, 1_000 + ttl + 1);
        assert_eq!(v, Verdict::TtlExpire);
    }

    /// The per-executor exclusion set and the disconnect/backstop paths:
    /// a bare disconnect charges nothing and requeues; the backstop
    /// charges `failed_builders` + `failure_count` and its own
    /// post-record threshold re-check poisons once three distinct
    /// workers have wedged.
    #[test]
    fn disconnect_charges_nothing_and_backstop_bounds_the_wedge_loop() {
        let d = |at, ex: &str| AttemptEvent::Disconnect {
            at,
            executor: ex.into(),
        };
        let b = |at, ex: &str| AttemptEvent::BackstopTimeout {
            at,
            executor: ex.into(),
        };
        // Disconnects alone never accumulate anything (contradiction C2:
        // the spec says they count; the code and therefore the fold say
        // they do not).
        let (c, v) = fold(&[d(1, "w1"), d(2, "w2"), d(3, "w3"), d(4, "w4")], 10);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);

        // Backstop timeouts do accumulate, and the third distinct worker
        // trips the threshold at the backstop's own re-check.
        let (c, v) = fold(&[b(1, "w1"), b(2, "w2"), b(3, "w3")], 10);
        assert_eq!(c.failed_builders.len(), 3);
        assert_eq!(c.failure_count, 3);
        assert_eq!(v, Verdict::Poison(PoisonReason::Threshold));

        // Two backstops then a disconnect: the disconnect's re-check
        // sees only two recorded failures and requeues.
        let (c, v) = fold(&[b(1, "w1"), b(2, "w2"), d(3, "w3")], 10);
        assert_eq!(c.failed_builders.len(), 2);
        assert_eq!(v, Verdict::Requeue);
    }

    /// The resubmit reset: a poisoned derivation resubmitted by the
    /// client gets a fresh per-cycle state with `resubmit_cycles`
    /// incremented; the second cycle's budget is the full `max_retries`
    /// again.
    #[test]
    fn resubmit_reset_restores_the_per_cycle_budget_and_counts_the_cycle() {
        let h = [
            t(100, "w1"),
            t(200, "w2"),
            t(300, "w3"), // poisoned via the threshold
            AttemptEvent::ResubmitReset { at: 400 },
            t(500, "w1"),
        ];
        let (c, v) = fold(&h[..4], 400);
        assert_eq!(c.resubmit_cycles, 1);
        assert_eq!(c.count, 0);
        assert_eq!(c.failed_builders.len(), 0);
        assert_eq!(c.poisoned_at, None);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&h, 500);
        assert_eq!(c.resubmit_cycles, 1);
        assert_eq!(c.count, 1, "fresh max_retries budget");
        assert_eq!(v, Verdict::Requeue);
    }

    /// The cache-hit clear wipes nine of the ten counters but preserves
    /// `backoff_until` exactly as the as-built in-place reset did; the
    /// admin clear / TTL removal wipes all ten.
    #[test]
    fn cache_hit_clear_preserves_backoff_and_poison_clear_does_not() {
        let h = [
            t(100, "w1"), // arms backoff_until = 105
            AttemptEvent::CacheHitClear { at: 200 },
        ];
        let (c, v) = fold(&h, 200);
        assert_eq!(c.count, 0);
        assert_eq!(c.failure_count, 0);
        assert_eq!(
            c.backoff_until,
            Some(105),
            "clear() touches 9 of the 10 fields"
        );
        assert_eq!(v, Verdict::Requeue);

        let h = [t(100, "w1"), AttemptEvent::PoisonCleared { at: 200 }];
        let (c, _) = fold(&h, 200);
        assert_eq!(c, Counters::default(), "the admin clear resets everything");
    }

    /// DIVERGENCE D2 + D3 (the controller-reported OOM path): an at-cap
    /// controller termination charges `infra_count` and — in the fold,
    /// deviating from the code — stamps the window anchor; a promoted
    /// one charges `exempt_infra_count` per `sched.retry.exempt-infra-cap`
    /// — also deviating from the code, which charges nothing. A
    /// neither-promoted-nor-at-cap one charges nothing in both.
    #[test]
    fn controller_termination_charges_per_floor_outcome() {
        let ct = |at, promoted, at_cap| AttemptEvent::ControllerTermination {
            at,
            executor: "w1".into(),
            promoted,
            at_cap,
        };
        let (c, v) = fold(&[ct(100, false, true)], 100);
        assert_eq!(c.infra_count, 1);
        assert_eq!(c.last_infra_failure_at, Some(100), "D2: the fold stamps");
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&[ct(100, true, false)], 100);
        assert_eq!(c.exempt_infra_count, 1, "D3: the fold charges");
        assert_eq!(c.infra_count, 0);
        assert_eq!(v, Verdict::Requeue);

        let (c, v) = fold(&[ct(100, false, false)], 100);
        assert_eq!(c, Counters::default());
        assert_eq!(v, Verdict::Requeue);

        // The at-cap arm shares the infra budget with E2: ten at-cap
        // controller OOMs then an eleventh poisons.
        let h: Vec<AttemptEvent> = (0..11).map(|i| ct(100 + i, false, true)).collect();
        let (c, v) = fold(&h, 200);
        assert_eq!(c.infra_count, 10);
        assert_eq!(v, Verdict::Poison(PoisonReason::InfraBudget));
    }

    /// The recovery projection: the four persisted counters come back,
    /// `failure_count` is derived from the set, and the five in-memory
    /// budgets are forgiven. The projection is not the fold of the
    /// pre-failover history — that is the documented lossy
    /// reconstruction, not a bug in either function.
    #[test]
    fn recovery_projection_is_the_documented_selective_forgiveness() {
        // Live history: two same-worker transient failures (count=2,
        // failure_count=2, one distinct builder), three infra failures,
        // one timeout.
        let h = [
            t(100, "w1"),
            t(200, "w1"),
            infra(300, "w1", false, false),
            infra(310, "w1", false, false),
            infra(320, "w1", false, false),
            AttemptEvent::WorkerTimeout {
                at: 400,
                executor: "w1".into(),
            },
        ];
        let (live, _) = fold(&h, 400);
        assert_eq!(live.count, 2);
        assert_eq!(live.failure_count, 2);
        assert_eq!(live.infra_count, 3);
        assert_eq!(live.timeout_count, 1);

        // What the mirror columns hold for that history: retry_count and
        // failed_builders are mirrored per-event; the rest are not
        // persisted at all.
        let persisted = PersistedRetryColumns {
            retry_count: live.count,
            resubmit_cycles: live.resubmit_cycles,
            failed_builders: live.failed_builders.clone(),
            poisoned_at: None,
        };
        let recovered = Counters::recovery_projection(&persisted);
        assert_eq!(recovered.count, 2, "recovered from the column");
        assert_eq!(
            recovered.failure_count, 1,
            "derived as failed_builders.len(): same-worker repeats forgotten"
        );
        assert_eq!(recovered.infra_count, 0, "forgiven");
        assert_eq!(recovered.timeout_count, 0, "forgiven");
        assert_eq!(recovered.last_infra_failure_at, None, "forgiven");
        assert_eq!(recovered.backoff_until, None, "forgiven");
        assert!(
            recovered.failure_count <= live.failure_count
                && recovered.failed_builders == live.failed_builders,
            "no fabrication: every recovered value is supported by a column"
        );
    }

    // -----------------------------------------------------------------
    // Phase-1b decision surface: decide() / classify() / placeable()
    // -----------------------------------------------------------------

    /// Test-side `AttemptRecord` builder: the fields the fold consumes,
    /// everything else defaulted. `at` is epoch seconds.
    fn rec(class: OutcomeClass, party: ReportingParty, executor: &str, at: u64) -> AttemptRecord {
        AttemptRecord {
            attempt_id: uuid::Uuid::now_v7(),
            event_kind: AttemptEventKind::Attempt,
            outcome_class: class,
            exec_id: None,
            executor_id: (!executor.is_empty()).then(|| ExecutorId::from(executor)),
            termination_reason: None,
            reporting_party: party,
            exempt: class == OutcomeClass::ExemptInfra,
            floor_promoted: false,
            floor_at_cap: false,
            error_msg: None,
            final_line_count: None,
            resubmit_cycle: 0,
            occurred_at_epoch_secs: at as f64,
            recorded_at_epoch_secs: at as f64,
        }
    }

    /// A reset-event record (`event_kind = 'reset'`).
    fn reset_rec(class: OutcomeClass, resubmit_cycle: i32, at: u64) -> AttemptRecord {
        AttemptRecord {
            event_kind: AttemptEventKind::Reset,
            resubmit_cycle,
            ..rec(class, ReportingParty::Scheduler, "", at)
        }
    }

    fn worker_rec(class: OutcomeClass, executor: &str, at: u64) -> AttemptRecord {
        rec(class, ReportingParty::Worker, executor, at)
    }

    fn decide_default(history: &[AttemptRecord], now: AbsTime) -> Decision {
        decide(history, &Budget::default(), now, None)
    }

    fn seed(retry_count: u32, resubmit_cycles: u32, failed: &[&str]) -> PersistedRetryColumns {
        PersistedRetryColumns {
            retry_count,
            resubmit_cycles,
            failed_builders: failed.iter().map(|s| s.to_string()).collect(),
            poisoned_at: None,
        }
    }

    /// An empty suffix with no seed is the default state: requeue,
    /// nothing excluded, no backoff.
    #[test]
    fn decide_empty_history_is_requeue() {
        let d = decide_default(&[], 0);
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(d.exclusion.is_empty());
        assert_eq!(d.backoff_until, None);
        assert_eq!(d.counters, Counters::default());
    }

    /// decide() over worker-reported ledger rows reproduces the
    /// reference fold's hand-computed transient-budget history: two
    /// same-worker retries with backoff, poison via the per-cycle cap on
    /// the third, and the executor lands in the exclusion set.
    #[test]
    fn decide_transient_budget_matches_the_reference_fold() {
        let h = [
            worker_rec(OutcomeClass::Transient, "w1", 100),
            worker_rec(OutcomeClass::Transient, "w1", 200),
            worker_rec(OutcomeClass::Transient, "w1", 300),
        ];
        let oracle_events = [t(100, "w1"), t(200, "w1"), t(300, "w1")];

        let d = decide_default(&h[..2], 200);
        let (oc, ov) = fold(&oracle_events[..2], 200);
        assert_eq!(d.counters, oc, "counters match the reference fold");
        assert_eq!(d.verdict, ov);
        assert_eq!(d.backoff_until, Some(210));
        assert!(d.exclusion.contains(&ExecutorId::from("w1")));

        let d = decide_default(&h, 300);
        let (oc, ov) = fold(&oracle_events, 300);
        assert_eq!(d.counters, oc);
        assert_eq!(d.verdict, ov);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::TransientBudget));
    }

    /// decide() over the worker infra family: the non-exempt budget is
    /// check-then-increment with the 300 s window, exactly as the fold's
    /// infra history; exempt rows charge only the exemption budget.
    #[test]
    fn decide_infra_budget_and_window_match_the_reference_fold() {
        let mut h: Vec<AttemptRecord> = (0..10)
            .map(|i| worker_rec(OutcomeClass::Infra, "w1", 100 + i))
            .collect();
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.infra_count, 10);
        assert_eq!(d.counters.last_infra_failure_at, Some(109));
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(
            d.exclusion.is_empty(),
            "infra failures never join the exclusion set"
        );

        h.push(worker_rec(OutcomeClass::Infra, "w1", 110));
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::InfraBudget));

        // Sparse failures past the window are forgiven before charging.
        let sparse = [
            worker_rec(OutcomeClass::Infra, "w1", 100),
            worker_rec(OutcomeClass::Infra, "w1", 452),
        ];
        let d = decide_default(&sparse, 452);
        assert_eq!(d.counters.infra_count, 1, "window reset then charge");
    }

    /// The worker timeout budget through decide(): four requeues then
    /// terminal Cancel, never Poisoned.
    #[test]
    fn decide_worker_timeout_cap_is_cancel() {
        let h: Vec<AttemptRecord> = (0..5)
            .map(|i| worker_rec(OutcomeClass::Timeout, "w1", 100 + i))
            .collect();
        let d = decide_default(&h[..4], 200);
        assert_eq!(d.counters.timeout_count, 4);
        assert_eq!(d.verdict, Verdict::Requeue);
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Cancel);
        assert_eq!(d.counters.poisoned_at, None);
    }

    /// A permanent failure poisons immediately and the TTL downgrade
    /// works through decide()'s `now`.
    #[test]
    fn decide_permanent_poisons_and_ttl_expires() {
        let h = [worker_rec(OutcomeClass::Permanent, "w1", 1_000)];
        let d = decide_default(&h, 1_000);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Permanent));
        let ttl = Budget::default().poison_ttl_secs;
        let d = decide_default(&h, 1_000 + ttl + 1);
        assert_eq!(d.verdict, Verdict::TtlExpire);
    }

    /// A promoted / CONCURRENT_PUTPATH exempt-infra history: classify()
    /// maps it to the exempt class, and decide() charges only
    /// `exempt_infra_count` — never the transient count, never the
    /// non-exempt infra count.
    #[test]
    fn decide_exempt_infra_history_charges_only_the_exemption_budget() {
        // classify(): both exemption vehicles map to ExemptInfra.
        assert_eq!(
            classify(
                &ObservedFailure::WorkerInfra { error_msg: "boom" },
                FloorOutcomeView {
                    promoted: true,
                    at_cap: false
                }
            ),
            OutcomeClass::ExemptInfra
        );
        let putpath_msg = format!("upload failed: {}", rio_proto::CONCURRENT_PUTPATH_MSG);
        assert_eq!(
            classify(
                &ObservedFailure::WorkerInfra {
                    error_msg: &putpath_msg
                },
                FloorOutcomeView::default()
            ),
            OutcomeClass::ExemptInfra
        );

        let h: Vec<AttemptRecord> = (0..3)
            .map(|i| worker_rec(OutcomeClass::ExemptInfra, "w1", 100 + i))
            .collect();
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.exempt_infra_count, 3);
        assert_eq!(d.counters.infra_count, 0);
        assert_eq!(d.counters.count, 0, "never the transient budget");
        assert_eq!(d.verdict, Verdict::Requeue);
    }

    /// The controller-classified rows: a promoted controller termination
    /// (class `exempt_infra`, scheduler/controller-reported) charges the
    /// exemption budget — divergence D3's adjudicated side, visible to
    /// any collapsed reader of the exempt cap; an unestablished
    /// `disconnected` row charges nothing.
    #[test]
    fn decide_controller_promoted_termination_charges_the_exemption_budget() {
        let h = [
            rec(
                OutcomeClass::ExemptInfra,
                ReportingParty::Scheduler,
                "w1",
                100,
            ),
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w1",
                110,
            ),
        ];
        let d = decide_default(&h, 120);
        assert_eq!(d.counters.exempt_infra_count, 1, "D3: the fold charges");
        assert_eq!(d.counters.infra_count, 0);
        assert_eq!(d.counters.failure_count, 0);
        assert_eq!(d.verdict, Verdict::Requeue);
    }

    /// Backstop rows charge the threshold/exclusion budget and the third
    /// distinct wedged worker poisons; cascade and fleet-exhaust marker
    /// rows are no-ops for the fold.
    #[test]
    fn decide_backstop_rows_bound_the_wedge_loop_and_markers_are_noops() {
        let h = [
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w1", 10),
            rec(OutcomeClass::Cascade, ReportingParty::Scheduler, "", 11),
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w2", 20),
            rec(
                OutcomeClass::FleetExhaust,
                ReportingParty::Scheduler,
                "",
                21,
            ),
            rec(OutcomeClass::Backstop, ReportingParty::Scheduler, "w3", 30),
        ];
        let d = decide_default(&h[..4], 25);
        assert_eq!(d.counters.failed_builders.len(), 2);
        assert_eq!(d.verdict, Verdict::Requeue);
        let d = decide_default(&h, 35);
        assert_eq!(d.counters.failed_builders.len(), 3);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(d.exclusion.len(), 3);
    }

    // r[verify sched.retry.per-executor-budget+2]
    /// C2 (T-1b.11): an `executor_crash` history charges the
    /// threshold/exclusion budget — each established crash joins
    /// `failed_builders` and increments `failure_count`, the placement
    /// exclusion picks the crashing executors up, and the third
    /// distinct establishment crosses the default threshold. A bare
    /// `disconnected` row (not yet established) still charges nothing.
    #[test]
    fn decide_executor_crash_history_charges_the_threshold_budget() {
        let ex = |ids: &[&str]| -> BTreeSet<ExecutorId> {
            ids.iter().map(|s| ExecutorId::from(*s)).collect()
        };
        let crash = |w: &str, at: u32| {
            rec(
                OutcomeClass::ExecutorCrash,
                ReportingParty::Scheduler,
                w,
                u64::from(at),
            )
        };
        // Two establishments: charged and excluded, still under the
        // threshold; the fold-view exclusion feeds placeable().
        let h = [crash("w0", 100), crash("w1", 101)];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.failure_count, 2);
        assert_eq!(d.counters.failed_builders.len(), 2);
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(d.exclusion.contains(&ExecutorId::from("w0")));
        assert!(d.exclusion.contains(&ExecutorId::from("w1")));
        assert_eq!(
            placeable(&d.exclusion, &ex(&["w0", "w1"])),
            Placement::FleetExhausted,
            "a fleet consisting only of crashed executors reads exhausted"
        );
        assert_eq!(
            placeable(&d.exclusion, &ex(&["w0", "w1", "w-fresh"])),
            Placement::Placeable,
            "a fresh executor keeps the derivation placeable"
        );

        // The third distinct establishment crosses the threshold (the
        // C2 boundedness the as-built code lacked).
        let h = [crash("w0", 100), crash("w1", 101), crash("w2", 102)];
        let d = decide_default(&h, 200);
        assert_eq!(d.verdict, Verdict::Poison(PoisonReason::Threshold));
        assert_eq!(d.counters.failed_builders.len(), 3);

        // Unestablished disconnects stay uncharged.
        let h = [
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w0",
                100,
            ),
            rec(
                OutcomeClass::Disconnected,
                ReportingParty::Scheduler,
                "w1",
                101,
            ),
        ];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters, Counters::default());
        assert!(d.exclusion.is_empty());
    }

    /// A suffix that begins with a resubmit-reset row seeds
    /// `resubmit_cycles` from the row's cycle index, and the per-cycle
    /// budget is fresh.
    #[test]
    fn decide_leading_resubmit_reset_seeds_the_cycle_index() {
        let h = [
            reset_rec(OutcomeClass::ResubmitReset, 2, 400),
            worker_rec(OutcomeClass::Transient, "w1", 500),
        ];
        let d = decide_default(&h, 500);
        assert_eq!(d.counters.resubmit_cycles, 2, "carried from the row");
        assert_eq!(d.counters.count, 1, "fresh per-cycle budget");
        assert_eq!(d.verdict, Verdict::Requeue);

        // Cache-hit and poison-clear reset rows behave as the fold's
        // clear events (no cycle carried).
        let h = [
            worker_rec(OutcomeClass::Transient, "w1", 100),
            reset_rec(OutcomeClass::CacheHitClear, 0, 200),
        ];
        let d = decide_default(&h, 200);
        assert_eq!(d.counters.count, 0);
        assert_eq!(d.counters.backoff_until, Some(105), "clear() keeps backoff");
    }

    /// P5 legacy seed, degenerate case: an empty suffix with non-empty
    /// mirror columns is exactly the documented recovery projection.
    #[test]
    fn decide_seed_only_degenerates_to_the_legacy_projection() {
        let s = seed(2, 1, &["legacy-a", "legacy-b"]);
        let d = decide(&[], &Budget::default(), 0, Some(&s));
        assert_eq!(d.counters, Counters::recovery_projection(&s));
        assert_eq!(d.verdict, Verdict::Requeue);
        assert_eq!(d.exclusion.len(), 2);
    }

    /// P5 legacy seed + post-066 rows: the threshold, the exclusion set,
    /// and the resubmit bound reflect both eras — a single new distinct
    /// failure on top of two legacy-era distinct failures crosses the
    /// distinct-worker threshold, and `count` is floored at the column
    /// value (max, not sum).
    #[test]
    fn decide_seed_plus_attempts_reflect_both_eras() {
        let s = seed(1, 1, &["legacy-a", "legacy-b"]);
        let h = [worker_rec(OutcomeClass::Transient, "w-new", 100)];
        let d = decide(&h, &Budget::default(), 100, Some(&s));
        assert_eq!(
            d.verdict,
            Verdict::Poison(PoisonReason::Threshold),
            "third distinct executor across the eras crosses the threshold"
        );
        assert_eq!(d.counters.failed_builders.len(), 3);
        assert_eq!(d.exclusion.len(), 3);
        assert!(d.exclusion.contains(&ExecutorId::from("legacy-a")));
        assert!(d.exclusion.contains(&ExecutorId::from("w-new")));
        assert_eq!(d.counters.resubmit_cycles, 1);
        assert!(
            d.counters.count >= 1,
            "count is floored at the column value (max, not sum)"
        );
    }

    /// P5 legacy seed: a suffix that contains a reset row ignores the
    /// seed entirely (the reset clears within-cycle state under both
    /// semantics and the reset row carries the cycle index itself).
    #[test]
    fn decide_seed_is_ignored_when_the_suffix_has_a_reset_row() {
        let s = seed(2, 1, &["legacy-a", "legacy-b"]);
        let h = [
            reset_rec(OutcomeClass::ResubmitReset, 2, 400),
            worker_rec(OutcomeClass::Transient, "w-new", 500),
        ];
        let d = decide(&h, &Budget::default(), 500, Some(&s));
        assert_eq!(
            d.counters.failed_builders.len(),
            1,
            "legacy executors do not leak past a reset row"
        );
        assert!(!d.exclusion.contains(&ExecutorId::from("legacy-a")));
        assert_eq!(d.counters.resubmit_cycles, 2, "from the reset row");
        assert_eq!(d.verdict, Verdict::Requeue);

        // An all-default seed is also a no-op even without a reset row.
        let empty = PersistedRetryColumns::default();
        let h = [worker_rec(OutcomeClass::Transient, "w-new", 100)];
        let with = decide(&h, &Budget::default(), 100, Some(&empty));
        let without = decide(&h, &Budget::default(), 100, None);
        assert_eq!(with.counters, without.counters);
        assert_eq!(with.verdict, without.verdict);
    }

    /// classify() is total over the trigger alphabet and never lets a
    /// transient failure consult the floor (P4).
    #[test]
    fn classify_maps_every_trigger_and_transients_never_consult_the_floor() {
        let promoted = FloorOutcomeView {
            promoted: true,
            at_cap: false,
        };
        let none = FloorOutcomeView::default();
        assert_eq!(
            classify(&ObservedFailure::WorkerTransient, promoted),
            OutcomeClass::Transient,
            "P4: a promoted floor never reclassifies a transient failure"
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerInfra { error_msg: "eio" }, none),
            OutcomeClass::Infra
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerPermanent, promoted),
            OutcomeClass::Permanent
        );
        assert_eq!(
            classify(&ObservedFailure::WorkerTimeout, promoted),
            OutcomeClass::Timeout
        );
        assert_eq!(
            classify(&ObservedFailure::Disconnect, none),
            OutcomeClass::Disconnected
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerResourceTermination, promoted),
            OutcomeClass::ExemptInfra
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerResourceTermination, none),
            OutcomeClass::Infra
        );
        assert_eq!(
            classify(&ObservedFailure::ControllerDeadlineExceeded, none),
            OutcomeClass::Timeout
        );
        assert_eq!(
            classify(&ObservedFailure::BackstopTimeout, none),
            OutcomeClass::Backstop
        );
        assert_eq!(
            classify(&ObservedFailure::UnreportedCrash, none),
            OutcomeClass::ExecutorCrash
        );
    }

    /// placeable(): the three-way placement verdict mirrors the as-built
    /// fleet-exhaust predicate, including both empty-set carve-outs.
    #[test]
    fn placeable_mirrors_the_fleet_exhaust_predicate() {
        let ex = |ids: &[&str]| -> BTreeSet<ExecutorId> {
            ids.iter().map(|s| ExecutorId::from(*s)).collect()
        };
        assert_eq!(
            placeable(&ex(&["w1", "w2"]), &ex(&["w1", "w2"])),
            Placement::FleetExhausted
        );
        assert_eq!(
            placeable(&ex(&["w1"]), &ex(&["w1", "w2"])),
            Placement::Placeable
        );
        assert_eq!(
            placeable(&ex(&[]), &ex(&["w1"])),
            Placement::Placeable,
            "nothing failed yet: never exhausted"
        );
        assert_eq!(
            placeable(&ex(&["w1", "w2"]), &ex(&[])),
            Placement::NoEligibleWorkers,
            "empty pool is a provisioning transient, never a poison"
        );
        // The exclusion set may contain workers no longer in the fleet.
        assert_eq!(
            placeable(&ex(&["gone-1", "gone-2"]), &ex(&["w-fresh"])),
            Placement::Placeable
        );
    }
}

#[cfg(kani)]
mod proofs {
    //! CBMC proof harnesses for the decision kernels (`decide` /
    //! `classify` / `placeable` and the fold's counter arithmetic).
    //!
    //! Domain bounds, stated once: histories are bounded at 3–4 records
    //! over a three-executor universe, budgets are scaled to 0..=2 (so
    //! every cap, threshold, and TTL terminal is reachable inside the
    //! bound — the same scaling `retryPolicy.qnt`'s regimes use), and
    //! the abstract clock is bounded at small values. The record domain
    //! is a strict superset of what the appending sites can write
    //! (arbitrary class/kind/flag/party combinations, including
    //! malformed ones the fold treats as no-ops), so proving over it is
    //! sound for the reachable subset. Counter arithmetic is +1 per
    //! event onto u32 with saturating clock math, so the length bound
    //! is a solver budget, not a hidden precondition — overflow within
    //! the bound is rejected by CBMC's overflow checks, and exceeding
    //! u32 in production would need ~4 × 10⁹ rows in one suffix, which
    //! the per-cycle suffix bound (≤ ~70 rows) excludes structurally.
    //!
    //! The tracey verify markers for these harnesses live at the
    //! `kani-rio-scheduler` wiring point in nix/kani.nix, not here —
    //! same discipline as the VM-test subtests list.

    use super::*;
    use crate::state::{AttemptEventKind, AttemptRecord, ExecutorId, OutcomeClass, ReportingParty};

    /// The executor-identity universe. Three distinct ids: enough to
    /// reach the distinct-worker threshold at its scaled bound (2) with
    /// one spare so exclusion ⊂ fleet and exclusion ⊇ fleet are both
    /// reachable in `check_placeable_contract`.
    const EXECUTORS: [&str; 3] = ["w0", "w1", "w2"];

    fn any_executor() -> &'static str {
        let i: u8 = kani::any();
        kani::assume(i < 3);
        EXECUTORS[i as usize]
    }

    fn any_outcome_class() -> OutcomeClass {
        let i: u8 = kani::any();
        kani::assume(i < 13);
        match i {
            0 => OutcomeClass::Transient,
            1 => OutcomeClass::Infra,
            2 => OutcomeClass::ExemptInfra,
            3 => OutcomeClass::Timeout,
            4 => OutcomeClass::Permanent,
            5 => OutcomeClass::Cascade,
            6 => OutcomeClass::Backstop,
            7 => OutcomeClass::Disconnected,
            8 => OutcomeClass::ExecutorCrash,
            9 => OutcomeClass::FleetExhaust,
            10 => OutcomeClass::ResubmitReset,
            11 => OutcomeClass::CacheHitClear,
            _ => OutcomeClass::PoisonCleared,
        }
    }

    fn any_reporting_party() -> ReportingParty {
        let i: u8 = kani::any();
        kani::assume(i < 4);
        match i {
            0 => ReportingParty::Worker,
            1 => ReportingParty::Controller,
            2 => ReportingParty::Scheduler,
            _ => ReportingParty::Admin,
        }
    }

    /// One arbitrary ledger record: every decision-relevant field free
    /// (class, kind, flags, party, executor, cycle, timestamp), every
    /// field `decide()` ignores pinned to a cheap concrete value.
    fn any_record(max_at: u64) -> AttemptRecord {
        let at: u64 = kani::any();
        kani::assume(at <= max_at);
        let cycle: i32 = kani::any();
        kani::assume((0..=3).contains(&cycle));
        AttemptRecord {
            attempt_id: uuid::Uuid::nil(),
            event_kind: if kani::any() {
                AttemptEventKind::Attempt
            } else {
                AttemptEventKind::Reset
            },
            outcome_class: any_outcome_class(),
            exec_id: None,
            executor_id: if kani::any() {
                Some(ExecutorId::from(any_executor()))
            } else {
                None
            },
            termination_reason: None,
            reporting_party: any_reporting_party(),
            exempt: kani::any(),
            floor_promoted: kani::any(),
            floor_at_cap: kani::any(),
            error_msg: None,
            final_line_count: None,
            resubmit_cycle: cycle,
            occurred_at_epoch_secs: at as f64,
            recorded_at_epoch_secs: 0.0,
        }
    }

    /// A bounded arbitrary suffix: up to `max_len` records.
    fn any_history(max_len: usize) -> Vec<AttemptRecord> {
        let n: usize = kani::any();
        kani::assume(n <= max_len);
        let mut h = Vec::new();
        let mut i = 0;
        while i < n {
            h.push(any_record(8));
            i += 1;
        }
        h
    }

    /// Budgets scaled so every terminal is reachable within the history
    /// bound. The contract must hold for every configuration, so zero
    /// caps and both threshold modes are included.
    fn any_small_budget() -> Budget {
        let small_u32 = |bound: u32| -> u32 {
            let v: u32 = kani::any();
            kani::assume(v <= bound);
            v
        };
        let small_u64 = |bound: u64| -> u64 {
            let v: u64 = kani::any();
            kani::assume(v <= bound);
            v
        };
        Budget {
            max_retries: small_u32(2),
            max_infra_retries: small_u32(2),
            max_timeout_retries: small_u32(2),
            max_exempt_infra_retries: small_u32(2),
            infra_retry_window_secs: small_u64(3),
            backoff_base_secs: small_u64(3),
            backoff_multiplier: small_u64(2),
            backoff_max_secs: small_u64(4),
            poison_threshold: small_u32(2),
            require_distinct_workers: kani::any(),
            poison_resubmit_retry_limit: small_u32(2),
            poison_ttl_secs: small_u64(3),
        }
    }

    /// An arbitrary frozen-mirror-column row (the P5 legacy seed). The
    /// flat counters are unconstrained u32 — the SQL loader bounds them
    /// at `i32::MAX`, but the proof does not need that bound.
    fn any_seed() -> PersistedRetryColumns {
        let mut failed_builders = BTreeSet::new();
        if kani::any() {
            failed_builders.insert(EXECUTORS[0].to_string());
        }
        if kani::any() {
            failed_builders.insert(EXECUTORS[1].to_string());
        }
        let poisoned_at = if kani::any() {
            let p: u64 = kani::any();
            kani::assume(p <= 8);
            Some(p)
        } else {
            None
        };
        PersistedRetryColumns {
            retry_count: kani::any(),
            resubmit_cycles: kani::any(),
            failed_builders,
            poisoned_at,
        }
    }

    /// Verify [`decide`] against its four `kani::ensures` contracts —
    /// the verdict partition, the Requeue cap bounds, the
    /// exclusion-set superset, and the legacy-seed floor — for every
    /// suffix of up to 4 arbitrary records, every scaled budget, every
    /// clock value up to 16, and every (or no) legacy seed. With
    /// overflow checks on, the same run is the no-overflow proof for
    /// the fold's counter arithmetic over that domain.
    #[kani::proof_for_contract(decide)]
    #[kani::unwind(9)]
    fn check_decide_contract() {
        let history = any_history(4);
        let budget = any_small_budget();
        let now: AbsTime = kani::any();
        kani::assume(now <= 16);
        let seed = if kani::any() { Some(any_seed()) } else { None };
        let _ = decide(&history, &budget, now, seed.as_ref());
    }

    /// The verdict partition is deterministic: two calls on the same
    /// (history, budget, now, seed) quadruple return the same Decision
    /// — no hidden state, no clock other than `now`, no dependence on
    /// set iteration order.
    #[kani::proof]
    #[kani::unwind(9)]
    fn check_decide_deterministic() {
        let history = any_history(2);
        let budget = any_small_budget();
        let now: AbsTime = kani::any();
        kani::assume(now <= 16);
        let seed = if kani::any() { Some(any_seed()) } else { None };
        let a = decide(&history, &budget, now, seed.as_ref());
        let b = decide(&history, &budget, now, seed.as_ref());
        assert_eq!(a, b);
    }

    /// The P5 legacy-seed merge, two-call form. With a reset-free
    /// suffix and a non-empty seed: the merge never drops legacy
    /// evidence (count / resubmit_cycles / failure_count are floored at
    /// the legacy projection and the exclusion set keeps every legacy
    /// member — re-checked here against `Counters::recovery_projection`
    /// so the floor and the projection stay in lockstep), never drops
    /// suffix evidence (the unseeded fold's exclusion set, failure
    /// count, and resubmit cycles are preserved), and never inflates a
    /// channel budget (infra / timeout / exempt counts are exactly the
    /// unseeded fold's). With a reset-bearing suffix or an empty legacy
    /// row, the seed is ignored entirely. The per-cycle `count` is NOT
    /// claimed monotone against the unseeded fold: a merged exclusion
    /// set can reach the poison threshold earlier, and the threshold
    /// arm poisons before the per-cycle charge — the evidence lands in
    /// `failed_builders` instead (the design's "never below what either
    /// era supports" floor is the legacy-projection half plus the
    /// preserved suffix exclusions, which are asserted).
    #[kani::proof]
    #[kani::unwind(9)]
    fn check_legacy_seed_merge_monotone() {
        let history = any_history(3);
        let budget = any_small_budget();
        let now: AbsTime = kani::any();
        kani::assume(now <= 16);
        let seed = any_seed();
        let seeded = decide(&history, &budget, now, Some(&seed));
        let unseeded = decide(&history, &budget, now, None);
        let has_reset = history
            .iter()
            .any(|r| r.event_kind == AttemptEventKind::Reset);
        if has_reset || seed.is_empty() {
            assert_eq!(seeded, unseeded);
        } else {
            // Legacy floor (the decide() ensures states this too; the
            // projection cross-check is what this harness adds).
            let proj = Counters::recovery_projection(&seed);
            assert!(seeded.counters.count >= proj.count);
            assert!(seeded.counters.resubmit_cycles >= proj.resubmit_cycles);
            assert!(seeded.counters.failure_count >= proj.failure_count);
            for w in proj.failed_builders.iter() {
                assert!(seeded.counters.failed_builders.contains(w));
            }
            // Suffix evidence preserved.
            assert!(seeded.counters.resubmit_cycles >= unseeded.counters.resubmit_cycles);
            assert!(seeded.counters.failure_count >= unseeded.counters.failure_count);
            for w in unseeded.counters.failed_builders.iter() {
                assert!(seeded.counters.failed_builders.contains(w));
            }
            // Channel budgets are seed-independent.
            assert_eq!(seeded.counters.infra_count, unseeded.counters.infra_count);
            assert_eq!(
                seeded.counters.timeout_count,
                unseeded.counters.timeout_count
            );
            assert_eq!(
                seeded.counters.exempt_infra_count,
                unseeded.counters.exempt_infra_count
            );
        }
    }

    /// Verify [`classify`] against its partition contract for every
    /// observed-failure variant, every floor outcome, and four
    /// representative error messages (empty, unrelated, the
    /// CONCURRENT_PUTPATH marker verbatim, the marker embedded
    /// mid-string) — the shapes the substring predicate distinguishes.
    #[kani::proof_for_contract(classify)]
    fn check_classify_contract() {
        let floor = FloorOutcomeView {
            promoted: kani::any(),
            at_cap: kani::any(),
        };
        let msg_sel: u8 = kani::any();
        kani::assume(msg_sel < 4);
        let error_msg = match msg_sel {
            0 => "",
            1 => "store error: connection reset by peer",
            2 => rio_proto::CONCURRENT_PUTPATH_MSG,
            _ => "remote: concurrent PutPath in progress (path locked)",
        };
        let ev_sel: u8 = kani::any();
        kani::assume(ev_sel < 9);
        let event = match ev_sel {
            0 => ObservedFailure::WorkerTransient,
            1 => ObservedFailure::WorkerInfra { error_msg },
            2 => ObservedFailure::WorkerPermanent,
            3 => ObservedFailure::WorkerTimeout,
            4 => ObservedFailure::Disconnect,
            5 => ObservedFailure::ControllerResourceTermination,
            6 => ObservedFailure::ControllerDeadlineExceeded,
            7 => ObservedFailure::BackstopTimeout,
            _ => ObservedFailure::UnreportedCrash,
        };
        let _ = classify(&event, floor);
    }

    /// One arbitrary subset of the executor universe.
    fn any_id_set() -> BTreeSet<ExecutorId> {
        let mut s = BTreeSet::new();
        if kani::any() {
            s.insert(ExecutorId::from(EXECUTORS[0]));
        }
        if kani::any() {
            s.insert(ExecutorId::from(EXECUTORS[1]));
        }
        if kani::any() {
            s.insert(ExecutorId::from(EXECUTORS[2]));
        }
        s
    }

    /// Verify [`placeable`] against its partition contract for every
    /// (exclusion, fleet) pair over the three-executor universe —
    /// including the empty fleet (defer, never poison), the empty
    /// exclusion set (always placeable), full overlap (exhausted), and
    /// every partial overlap.
    #[kani::proof_for_contract(placeable)]
    #[kani::unwind(5)]
    fn check_placeable_contract() {
        let excluded = any_id_set();
        let eligible = any_id_set();
        let _ = placeable(&excluded, &eligible);
    }

    /// The fleet-exhaust arm of the fold itself (E1's check, which
    /// `decide()` deliberately never exercises — it folds against an
    /// empty fleet): over histories of up to 3 worker-reported events
    /// and every fleet subset, a `FleetExhausted` poison requires a
    /// non-empty eligible fleet whose every member has already failed
    /// this derivation, and an empty fleet never produces one (the
    /// empty-fleet defer clause of `sched.dispatch.fleet-exhaust+3`).
    #[kani::proof]
    #[kani::unwind(5)]
    fn check_fold_fleet_exhaust_arm() {
        let n: usize = kani::any();
        kani::assume(n <= 3);
        let mut history = Vec::new();
        let mut i = 0;
        while i < n {
            let at: u64 = kani::any();
            kani::assume(at <= 8);
            let ev = if kani::any() {
                AttemptEvent::Transient {
                    at,
                    executor: any_executor().to_string(),
                }
            } else {
                AttemptEvent::Infra {
                    at,
                    executor: any_executor().to_string(),
                    exempt: kani::any(),
                    at_cap: kani::any(),
                }
            };
            history.push(ev);
            i += 1;
        }
        let mut fleet = FleetView::default();
        if kani::any() {
            fleet.eligible.insert(EXECUTORS[0].to_string());
        }
        if kani::any() {
            fleet.eligible.insert(EXECUTORS[1].to_string());
        }
        let now: AbsTime = kani::any();
        kani::assume(now <= 16);
        let (counters, verdict) = reference_fold(&history, now, &Budget::default(), &fleet);
        if verdict == Verdict::Poison(PoisonReason::FleetExhausted) {
            assert!(!fleet.eligible.is_empty());
            for w in fleet.eligible.iter() {
                assert!(counters.failed_builders.contains(w));
            }
        }
        if fleet.eligible.is_empty() {
            assert!(verdict != Verdict::Poison(PoisonReason::FleetExhausted));
        }
    }
}
