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
//! **This module is dead code until Phase 1.** Nothing in the actor calls
//! it. It exists so that (a) the `retryPolicy.qnt` model's
//! `CountersRefineHistory` invariant has a precise definition to compare
//! the live counters against, (b) the divergences between the nine entry
//! points are pinned as executable adjudications rather than prose, and
//! (c) Phase 1's `decide()` has a tested starting point. It is wired as a
//! module (rather than left on disk unreferenced) so clippy, rustfmt, and
//! the unit tests run against it; `#[allow(dead_code)]` on the `mod` item
//! suppresses the unused-item lint until Phase 1 wires it into the
//! decision sites.
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
//! - Executor identities are plain `String`s so the module stays a leaf
//!   (no dependency on the actor, the DAG, the state machine, or tokio).
//! - The fold assumes the derivation is in a dispatchable, non-terminal
//!   state when each event arrives — the entry points' "is the node still
//!   poison-able" status guards are upstream of the accounting, and an
//!   event that was dropped by such a guard is simply absent from the
//!   observed history.

use std::collections::HashSet;

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
    pub eligible: HashSet<String>,
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
    /// `RetryState::clear()` on a cache-hit transition out of
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
    pub failed_builders: HashSet<String>,
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

    /// `RetryState::clear()` — wipes nine of the ten fields.
    /// `backoff_until` deliberately survives (the production `clear()`
    /// does not touch it).
    fn clear_for_cache_hit(&mut self) {
        let backoff = self.backoff_until;
        *self = Self {
            backoff_until: backoff,
            ..Self::default()
        };
    }

    // r[impl sched.retry.recovery-projection]
    /// The documented post-failover projection of the persisted columns:
    /// 4 counters recovered, `failure_count` derived as
    /// `failed_builders.len()`, the remaining 5 reset to defaults. This
    /// is `from_recovery_row` / `from_poisoned_row`'s retry-state
    /// construction as a pure function, so the model's failover action
    /// and the calibration's G8 reverts have an executable definition of
    /// "the documented selective forgiveness".
    ///
    /// The projection is *not* the fold of the pre-failover history:
    /// `failure_count` both forgets same-worker repeats (the live counter
    /// counts them) and counts the permanent path's diagnostics-only
    /// `failed_builders` insert (the live counter never charged it), and
    /// `failed_builders` itself is missing every backstop-recorded
    /// failure (divergence D4: E8 never mirrors its insert to PG). The
    /// no-fabrication bound is that every recovered value is supported by
    /// a persisted column — nothing is invented.
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
    pub failed_builders: HashSet<String>,
    /// `derivations.poisoned_at`, already converted to the abstract
    /// clock and already filtered for TTL expiry.
    pub poisoned_at: Option<AbsTime>,
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
pub(crate) fn exhausts_fleet(failed_builders: &HashSet<String>, fleet: &FleetView) -> bool {
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
    let mut c = Counters::default();
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
        // r[impl sched.timeout.promote-on-exceed+2]
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
                // `sched.timeout.promote-on-exceed+2` names `Cancelled`
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

        // ── assign_to_worker ────────────────────────────────────────
        AttemptEvent::Dispatched { .. } => {
            c.backoff_until = None;
            Verdict::Requeue
        }

        // ── dag::merge resubmit reset ───────────────────────────────
        // r[impl sched.merge.poisoned-resubmit-bounded+2]
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
    /// `backoff_until` exactly as `RetryState::clear()` does; the admin
    /// clear / TTL removal wipes all ten.
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
}
