//! Pure decision kernels for the scheduler's retry/poison machinery.
//!
//! This crate is the dependency-light home of the retry-formal
//! campaign's decision surface: the reference fold over a derivation's
//! observed failure history ([`reference_fold`]), the Phase-1b decision
//! functions layered on it ([`decide`] / [`classify`] / [`placeable`]),
//! and their CBMC function contracts and proof harnesses
//! (`#[cfg(kani)] mod proofs`).
//!
//! ## Why a separate crate
//!
//! Everything here used to live in `rio-scheduler/src/retry_policy.rs`.
//! The logic is unchanged by the move — `rio_scheduler::retry_policy`
//! is now a thin projection shim that re-exports these types and maps
//! the scheduler's vocabulary (`AttemptRecord`, `ExecutorId`, the
//! sqlx-backed enums) onto the kernel's — but the verification economics
//! are not: a kani proof harness's goto model closes over its host
//! crate's artifact context, and inside rio-scheduler that context
//! carries Arc-backed identifiers, f64 timestamp conversions and the
//! crate's full reachable code, which pushed every harness past a
//! merge-gate CBMC budget. In this crate the harnesses' call graph is
//! exactly the kernel: no dependencies, no `Arc`, no floats, no I/O.
//! The same tactic as `rio-store/src/logs/kernel.rs` (the log
//! campaign's kernel split) and `rio-lease`'s `decide_pure()`, applied
//! at crate granularity. Keep it that way — this crate must not grow
//! dependencies.
//!
//! ## Executor identities are a type parameter
//!
//! The fold's per-executor state (`failed_builders`, the exclusion set,
//! the eligible fleet) is generic over the identity type `Id`. The
//! scheduler instantiates it with `String` (the fold vocabulary it has
//! always used) and with its `Arc<str>`-backed `ExecutorId` for the
//! placement predicate; the proof harnesses instantiate it with a small
//! copy type so the solver never has to model heap-allocated string
//! comparisons. The kernels only ever use `Ord`/`Eq`/`Clone`
//! of `Id`, so the choice of identity type cannot change a verdict.
//!
//! ## The exclusion-set representation is cfg(kani)-swapped
//!
//! Every executor-id set in the kernel (the per-executor exclusion set,
//! the eligible fleet) is declared as
//! [`IdSet`], an alias that resolves to `std::collections::BTreeSet`
//! everywhere except under `cfg(kani)`, where it resolves to
//! [`BoundedIdSet`] — a fixed-capacity array set whose operations are
//! short, concretely-bounded loops. Production semantics and the public
//! API are untouched (outside the kani cfg the alias *is* `BTreeSet`);
//! the proof harnesses get a goto model with no b-tree node machinery
//! in it, which is what lets them converge inside a merge-gate budget.
//! The two representations are pinned to each other by the differential
//! unit tests in `mod tests` and the set-semantics harness in
//! `mod proofs`.
//!
//! ## What `reference_fold` is
//!
//! [`reference_fold`] is a pure function from an observed failure-event
//! history to the ten `RetryState` counters and the budget verdict. It
//! is the executable specification of what the seventeen `RetryState`
//! mutation sites and nine cap-check entry points (E1–E9 in
//! `docs/spec/models/retry-invariant-map.md`) collectively implement:
//! which event charges which counter, the 300 s sliding-window reset,
//! the resource-floor `{promoted, at_cap}` exemption, the cache-hit and
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
//!   poison threshold (see the comment at the cap check in the fold's
//!   `apply` step); it stays because `sched.retry.transient-budget`'s
//!   final clause mandates it and non-distinct/dev configurations still
//!   reach it.
//! - **P4 (floor-promotion exemption, the c13f6a277 / I-213 class):**
//!   the exemption is infra-class only. [`classify`] maps a
//!   worker-reported infra failure with `floor_outcome.promoted` or a
//!   CONCURRENT_PUTPATH message — and a promoted controller
//!   termination — to the exempt infra class; a `TransientFailure`
//!   classifies as `transient` with **no floor outcome consulted**, and
//!   the transient arm carries no exemption (the as-built
//!   `handle_transient_failure` has no floor/promotion guard).
//!   Regression coverage for the promotion-exempt ladder stays with the
//!   existing `sched.retry.promotion-exempt+3` unit tests in
//!   rio-scheduler (`test_transient_failure_promotion_exempt_from_max_retries`);
//!   the floor oracle is NOT extended to transient events and the model
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
//! (`sched.retry.counters-refine-history+2`) and that the verdict is
//! the same for every observation of one physical history
//! (`sched.retry.verdict-channel-invariant`).
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
//! - The fold assumes the derivation is in a dispatchable, non-terminal
//!   state when each event arrives — the entry points' "is the node still
//!   poison-able" status guards are upstream of the accounting, and an
//!   event that was dropped by such a guard is simply absent from the
//!   observed history.

/// Abstract monotonic clock, in whole seconds since an arbitrary origin.
pub type AbsTime = u64;

// ---------------------------------------------------------------------------
// The exclusion-set representation
//
// Production code and the unit-test batteries use `std::collections::
// BTreeSet` for every executor-id set (the per-executor exclusion set, the
// eligible fleet). The CBMC proof harnesses do
// not: a `BTreeSet` insert/lookup walks the b-tree node machinery
// (`search_tree`, `find_key_index`, `insert_recursing`), and the symbolic
// execution of that code — not SAT solving — is what kept the decision
// harnesses from converging inside a merge-gate budget. Under `cfg(kani)`
// the `IdSet` alias swaps every one of those sets for `BoundedIdSet`, a
// fixed-capacity array set whose operations are short loops with concrete
// bounds (the proof-only-representation pattern: concrete structure,
// symbolic values). The swap is invisible outside the proofs — the alias
// resolves to `BTreeSet` under every other cfg, so production semantics,
// the public API and the scheduler shim are untouched. The two
// representations are pinned to each other by the differential unit tests
// in `mod tests` (exhaustive small insert sequences compared against
// `BTreeSet`) and by the `check_bounded_set_models_set_semantics` harness
// (the set axioms over symbolic values).
// ---------------------------------------------------------------------------

/// The executor-id set representation used by every kernel structure:
/// the production `BTreeSet` under every cfg except `kani`.
#[cfg(not(kani))]
pub type IdSet<Id> = std::collections::BTreeSet<Id>;

/// The executor-id set representation used by every kernel structure:
/// under `cfg(kani)` the bounded proof-time set, so the harnesses' goto
/// model never has to symbolically execute b-tree node code.
#[cfg(kani)]
pub type IdSet<Id> = BoundedIdSet<Id>;

/// Capacity of [`BoundedIdSet`]. The proof harnesses' executor universe
/// is exactly four values (three named executors plus the "no executor
/// recorded" default), and every set-op loop and unwind bound scales
/// with this constant, so it is kept at exactly that universe. A
/// harness that ever widens its universe hits the insert-overflow panic
/// (a CBMC verification failure, never a silent drop) and forces this
/// constant — and the harness unwind bounds — up with it.
pub const BOUNDED_ID_SET_CAPACITY: usize = 4;

/// A fixed-capacity set with linear-scan membership — the proof-time
/// executor-id set representation (see [`IdSet`]).
///
/// Every operation is a plain index loop bounded by the concrete
/// [`BOUNDED_ID_SET_CAPACITY`], with no heap allocation, no node
/// structure, and no iterator-adapter chains (CBMC pays per closure and
/// per adapter state it has to symbolically execute, so the
/// implementation deliberately stays at the level of array indexing and
/// integer comparisons). Inserting more distinct values than the
/// capacity panics — within the harnesses' bounded domains the panic is
/// unreachable (every harness proves that as a side effect), and the
/// panic rather than a silent drop is what keeps the equivalence with
/// `BTreeSet` honest.
///
/// The type is compiled (and differentially unit-tested against
/// `BTreeSet`) under every cfg; only the [`IdSet`] alias is
/// cfg-dependent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedIdSet<Id> {
    /// Occupied slots, packed at the front in insertion order (the
    /// kernel only ever inserts; nothing removes a single element).
    slots: [Option<Id>; BOUNDED_ID_SET_CAPACITY],
}

impl<Id> Default for BoundedIdSet<Id> {
    fn default() -> Self {
        Self {
            slots: [const { None }; BOUNDED_ID_SET_CAPACITY],
        }
    }
}

impl<Id> BoundedIdSet<Id> {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of elements in the set.
    pub fn len(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < BOUNDED_ID_SET_CAPACITY {
            if self.slots[i].is_some() {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Whether the set has no elements.
    pub fn is_empty(&self) -> bool {
        let mut i = 0;
        while i < BOUNDED_ID_SET_CAPACITY {
            if self.slots[i].is_some() {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Iterate the elements in insertion order.
    pub fn iter(&self) -> BoundedIdSetIter<'_, Id> {
        BoundedIdSetIter {
            slots: &self.slots,
            next_index: 0,
        }
    }
}

impl<Id: Ord> BoundedIdSet<Id> {
    /// Whether `value` is in the set.
    pub fn contains(&self, value: &Id) -> bool {
        let mut i = 0;
        while i < BOUNDED_ID_SET_CAPACITY {
            if let Some(v) = &self.slots[i]
                && v == value
            {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Whether every element of `self` is also in `other` — the
    /// `BTreeSet::is_subset` surface [`exhausts_fleet`] and
    /// [`placeable`] consume.
    pub fn is_subset(&self, other: &Self) -> bool {
        let mut i = 0;
        while i < BOUNDED_ID_SET_CAPACITY {
            if let Some(v) = &self.slots[i]
                && !other.contains(v)
            {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Insert `value`; returns `true` iff it was not already present
    /// (the `BTreeSet::insert` convention).
    ///
    /// # Panics
    ///
    /// If the set already holds [`BOUNDED_ID_SET_CAPACITY`] distinct
    /// values. The proof harnesses' domains stay below the capacity;
    /// the panic (a CBMC verification failure if ever reachable) is the
    /// tripwire that forces the capacity up rather than silently
    /// diverging from set semantics.
    pub fn insert(&mut self, value: Id) -> bool {
        if self.contains(&value) {
            return false;
        }
        let mut i = 0;
        while i < BOUNDED_ID_SET_CAPACITY {
            if self.slots[i].is_none() {
                self.slots[i] = Some(value);
                return true;
            }
            i += 1;
        }
        panic!("BoundedIdSet capacity exceeded: raise BOUNDED_ID_SET_CAPACITY");
    }
}

/// Iterator over a [`BoundedIdSet`]'s elements in insertion order. The
/// `next()` is a plain index scan (no adapter chain) so the membership
/// folds in the kernel, the contracts, and the harnesses stay cheap to
/// symbolically execute.
pub struct BoundedIdSetIter<'a, Id> {
    slots: &'a [Option<Id>; BOUNDED_ID_SET_CAPACITY],
    next_index: usize,
}

impl<'a, Id> Iterator for BoundedIdSetIter<'a, Id> {
    type Item = &'a Id;

    fn next(&mut self) -> Option<&'a Id> {
        while self.next_index < BOUNDED_ID_SET_CAPACITY {
            let slot = &self.slots[self.next_index];
            self.next_index += 1;
            if let Some(v) = slot {
                return Some(v);
            }
        }
        None
    }
}

impl<Id: Ord> FromIterator<Id> for BoundedIdSet<Id> {
    fn from_iter<T: IntoIterator<Item = Id>>(iter: T) -> Self {
        let mut set = Self::new();
        for value in iter {
            set.insert(value);
        }
        set
    }
}

impl<Id: Ord> Extend<Id> for BoundedIdSet<Id> {
    fn extend<T: IntoIterator<Item = Id>>(&mut self, iter: T) {
        for value in iter {
            self.insert(value);
        }
    }
}

/// The store-side error-message marker for a concurrent `PutPath` upload
/// race. [`classify`]'s exemption predicate greps worker-reported infra
/// error messages for it (`sched.retry.exempt-infra-cap`'s
/// CONCURRENT_PUTPATH half).
///
/// This is a transcription of `rio_proto::CONCURRENT_PUTPATH_MSG` — the
/// wire constant the store emits and rio-builder retries on. The kernel
/// cannot depend on rio-proto (it must stay dependency-free for the CBMC
/// harnesses), so the value is duplicated here and pinned in lockstep by
/// `concurrent_putpath_marker_matches_rio_proto` in
/// `rio-scheduler/src/retry_policy.rs` — change both together.
pub const CONCURRENT_PUTPATH_MSG: &str = "concurrent PutPath in progress";

/// Whether a worker-reported error message carries the
/// [`CONCURRENT_PUTPATH_MSG`] marker — the substring half of
/// [`classify`]'s exemption predicate.
///
/// This is a plain windowed byte comparison rather than `str::contains`:
/// the verdict is identical for this fixed, non-empty needle, but the
/// standard library's substring searcher (the two-way algorithm and its
/// `memcmp` fast paths) is expensive to symbolically execute, and this
/// predicate sits inside both [`classify`] and its CBMC contract. The
/// agreement with `str::contains` is pinned by the
/// `concurrent_putpath_predicate_matches_std_contains` unit test.
fn contains_concurrent_putpath_marker(msg: &str) -> bool {
    let hay = msg.as_bytes();
    let needle = CONCURRENT_PUTPATH_MSG.as_bytes();
    if hay.len() < needle.len() {
        return false;
    }
    let last_start = hay.len() - needle.len();
    let mut start = 0;
    while start <= last_start {
        let mut offset = 0;
        while offset < needle.len() && hay[start + offset] == needle[offset] {
            offset += 1;
        }
        if offset == needle.len() {
            return true;
        }
        start += 1;
    }
    false
}

/// The retry/poison budget — the union of `RetryPolicy`, `PoisonConfig`,
/// `POISON_RESUBMIT_RETRY_LIMIT`, and `POISON_TTL`, flattened into one
/// plain struct so the fold has a single configuration argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetView<Id> {
    /// Statically-eligible, non-draining, registered executor ids.
    pub eligible: IdSet<Id>,
}

impl<Id> Default for FleetView<Id> {
    fn default() -> Self {
        Self {
            eligible: IdSet::new(),
        }
    }
}

/// One observed accounting event. The variants are the nine entry points'
/// triggers plus the reset events and the dispatch (which clears the
/// backoff defer). Every variant carries its observation time `at`.
///
/// The executor identity is the attempt's exclusion/budget key — since
/// decision P12 that is the controller-authoritative source node (or
/// the equivalent attested identity), carried as `Option<Id>`: an event
/// whose row has no recorded identity (a pull attempt whose binding ack
/// never landed, or a legacy row) charges its flat counters exactly the
/// same but contributes nothing to the per-executor exclusion set or
/// the distinct-source poison threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEvent<Id> {
    /// E1 — worker `CompletionReport{TransientFailure}` (the build ran
    /// and exited non-zero) or `Unspecified`.
    Transient { at: AbsTime, executor: Option<Id> },
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
        executor: Option<Id>,
        exempt: bool,
        at_cap: bool,
    },
    /// E3 — one of the seven permanent statuses (`PermanentFailure`,
    /// `CachedFailure`, `DependencyFailed`, `LogLimitExceeded`,
    /// `OutputRejected`, `NotDeterministic`, `InputRejected`).
    Permanent { at: AbsTime, executor: Option<Id> },
    /// E4 — worker `CompletionReport{TimedOut}`.
    WorkerTimeout { at: AbsTime, executor: Option<Id> },
    /// E5 — gRPC stream disconnect, heartbeat timeout, or force-drain.
    /// Charges nothing; re-checks the poison threshold over failures
    /// recorded by other events.
    Disconnect { at: AbsTime, executor: Option<Id> },
    /// E6 — controller `ReportExecutorTermination{OomKilled,
    /// EvictedDiskPressure}`, correlated back to this derivation through
    /// `recently_disconnected` (or the race-ahead live-executor lookup).
    /// `promoted` / `at_cap` are `bump_resource_floor`'s outcome for the
    /// reported dimension.
    ControllerTermination {
        at: AbsTime,
        executor: Option<Id>,
        promoted: bool,
        at_cap: bool,
    },
    /// E7 — controller `ReportExecutorTermination{DeadlineExceeded}`,
    /// prefix-matched back to this derivation.
    ControllerDeadlineExceeded { at: AbsTime, executor: Option<Id> },
    /// E8 — the scheduler-side backstop timer: Running for longer than
    /// `max(est × 3, daemon_timeout + slack)` with no report.
    BackstopTimeout { at: AbsTime, executor: Option<Id> },
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
    EstablishedCrash { at: AbsTime, executor: Option<Id> },
    /// A successful dispatch. Clears `backoff_until`
    /// (the pull-mint delivery).
    Dispatched { at: AbsTime, executor: Option<Id> },
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

impl<Id> AttemptEvent<Id> {
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
/// field mirror of the scheduler's `RetryState` with the two `Instant`
/// fields as [`AbsTime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counters<Id> {
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
    /// Drives the placement exclusion ([`placeable`] / the spawn-intent
    /// exclusion), the distinct-workers poison threshold, and the
    /// fleet-exhaust check.
    pub failed_builders: IdSet<Id>,
    /// Flat failure count for `require_distinct_workers = false`
    /// (`RetryState::failure_count`).
    pub failure_count: u32,
    /// When the derivation was poisoned (`RetryState::poisoned_at`).
    pub poisoned_at: Option<AbsTime>,
    /// Earliest re-dispatch time (`RetryState::backoff_until`).
    pub backoff_until: Option<AbsTime>,
}

impl<Id> Default for Counters<Id> {
    fn default() -> Self {
        Self {
            count: 0,
            resubmit_cycles: 0,
            infra_count: 0,
            timeout_count: 0,
            last_infra_failure_at: None,
            exempt_infra_count: 0,
            failed_builders: IdSet::new(),
            failure_count: 0,
            poisoned_at: None,
            backoff_until: None,
        }
    }
}

impl<Id: Ord> Counters<Id> {
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
}

/// Which budget's exhaustion produced a `Poison` verdict. The production
/// poison reason is a free-form string (synthesized on some paths,
/// carrying the worker's error message on others — divergence A8); the
/// fold carries the discriminant only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonReason {
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
pub enum Verdict {
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

// r[impl sched.dispatch.fleet-exhaust+5]
/// The fleet-exhaust predicate shared by E1's poison check and E9's
/// dispatch-time backstop: every statically-eligible non-draining
/// registered worker has already failed this derivation. Returns `false`
/// when the exclusion set is empty (nothing has failed) or the eligible
/// fleet is empty (no pool is connected — that is a transient the
/// autoscaler handles, and poisoning would brick builds during a
/// rollout).
pub fn exhausts_fleet<Id: Ord>(failed_builders: &IdSet<Id>, fleet: &FleetView<Id>) -> bool {
    if failed_builders.is_empty() || fleet.eligible.is_empty() {
        return false;
    }
    fleet.eligible.is_subset(failed_builders)
}

// r[impl sched.retry.counters-refine-history+2]
// r[impl sched.retry.transient-budget+2]
// r[impl sched.retry.attempts-bounded+4]
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
pub fn reference_fold<Id: Ord + Clone>(
    history: &[AttemptEvent<Id>],
    now: AbsTime,
    budget: &Budget,
    fleet: &FleetView<Id>,
) -> (Counters<Id>, Verdict) {
    fold_events(Counters::default(), history, now, budget, fleet)
}

/// The fold body shared by [`reference_fold`]: apply every event in
/// order, then downgrade a stale poison to [`Verdict::TtlExpire`].
/// ([`decide`] folds ledger rows directly through the same [`apply`]
/// arms instead of materializing an event buffer.)
fn fold_events<Id: Ord + Clone>(
    initial: Counters<Id>,
    history: &[AttemptEvent<Id>],
    now: AbsTime,
    budget: &Budget,
    fleet: &FleetView<Id>,
) -> (Counters<Id>, Verdict) {
    let mut c = initial;
    let mut verdict = Verdict::Requeue;

    for ev in history {
        verdict = apply(&mut c, ev, budget, fleet);
    }

    let verdict = ttl_downgrade(&c, verdict, now, budget);
    (c, verdict)
}

// r[impl sched.state.poisoned-ttl]
/// Downgrade a stale poison to [`Verdict::TtlExpire`]. The TTL is a
/// property of (`poisoned_at`, `now`), not of the event sequence:
/// `tick_process_expired_poisons` discovers it by scanning, not by
/// receiving an event. Shared by [`fold_events`] and [`decide`] (which
/// folds ledger rows directly instead of materializing an intermediate
/// event buffer).
fn ttl_downgrade<Id>(c: &Counters<Id>, verdict: Verdict, now: AbsTime, budget: &Budget) -> Verdict {
    if matches!(verdict, Verdict::Poison(_))
        && let Some(p) = c.poisoned_at
        && now.saturating_sub(p) > budget.poison_ttl_secs
    {
        Verdict::TtlExpire
    } else {
        verdict
    }
}

/// Apply one event to the counters and return the verdict it produces.
/// Each arm cites the entry point it transcribes; the `DIVERGENCE` arms
/// deliberately deviate from the code per the invariant map.
fn apply<Id: Ord + Clone>(
    c: &mut Counters<Id>,
    ev: &AttemptEvent<Id>,
    budget: &Budget,
    fleet: &FleetView<Id>,
) -> Verdict {
    match ev {
        // ── E1: handle_transient_failure ────────────────────────────
        // Order matters and is the code's: record the failure first
        // (insert + increment), then check the poison threshold over the
        // set that now includes this failure, then check the fleet, then
        // check the per-cycle count cap, and only on the retry arm
        // increment `count` and arm the backoff.
        AttemptEvent::Transient { at, executor } => {
            if let Some(executor) = executor {
                c.failed_builders.insert(executor.clone());
            }
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
            // incremented here (asymmetry A6, kept as-built).
            if let Some(executor) = executor {
                c.failed_builders.insert(executor.clone());
            }
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

        // ── E8: backstop-timeout rows (historical) ───────────────────
        // The stream-era scheduler backstop (tick_process_backstop_timeouts)
        // was deleted with the session machinery; no production site
        // constructs this event anymore. The arm stays so folds over a
        // durable attempt history that contains pre-deletion backstop rows
        // keep reproducing the charge those rows carried (insert +
        // increment, then the threshold re-check sees it).
        AttemptEvent::BackstopTimeout { at, executor } => {
            if let Some(executor) = executor {
                c.failed_builders.insert(executor.clone());
            }
            c.failure_count += 1;
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::Threshold)
            } else {
                Verdict::Requeue
            }
        }

        // ── The establishment sweep (C2, Phase 1b T-1b.11) ──────────
        // r[impl sched.retry.per-executor-budget+4]
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
            if let Some(executor) = executor {
                c.failed_builders.insert(executor.clone());
            }
            c.failure_count += 1;
            if c.poison_threshold_reached(budget) {
                c.poisoned_at = Some(*at);
                Verdict::Poison(PoisonReason::Threshold)
            } else {
                Verdict::Requeue
            }
        }

        // ── dispatched (pull-mint delivery) ─────────────────────────
        AttemptEvent::Dispatched { .. } => {
            c.backoff_until = None;
            Verdict::Requeue
        }

        // ── dag::merge resubmit reset ───────────────────────────────
        // r[impl sched.merge.poisoned-resubmit-bounded+4]
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
// The ledger-row vocabulary
//
// A decision-relevant projection of one `drv_attempts` row. The scheduler's
// `AttemptRecord` (rio-scheduler/src/state/derivation.rs) is the full
// in-memory mirror of the row — UUIDs, error messages, f64 timestamps,
// Arc-backed executor ids; `LedgerRow` carries exactly the fields
// `decide()` consumes, with the timestamp already on the abstract clock.
// rio_scheduler::retry_policy::decide() owns the AttemptRecord → LedgerRow
// projection; the enums below mirror the scheduler's sqlx-backed db enums
// variant-for-variant (the shim's exhaustive `match`es fail to compile if
// either side gains a variant the other lacks).
// ---------------------------------------------------------------------------

/// Row kind in the durable attempt ledger (`drv_attempts.event_kind`,
/// migration 068): an observed attempt/charge event, or a reset event
/// (resubmit reset, cache-hit clear, poison clear) that starts a new
/// suffix for the fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptEventKind {
    /// An observed attempt/charge event.
    Attempt,
    /// A reset event (resubmit reset, cache-hit clear, poison clear).
    Reset,
}

/// Outcome classification of one attempt-ledger row
/// (`drv_attempts.outcome_class`, migration 068). This is the
/// [`classify`] alphabet — the kernel-side mirror of the scheduler's
/// `OutcomeClass` db enum (which owns the SQL string round-trip and the
/// migration CHECK-constraint lockstep test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeClass {
    /// E1 — worker-reported `TransientFailure` (build ran, exited
    /// non-zero) or `Unspecified`.
    Transient,
    /// E2 — worker-reported `InfrastructureFailure`, non-exempt.
    Infra,
    /// E2/E6 — infra failure exempt from the non-exempt cap
    /// (floor-promoted CgroupOom/OOMKilled, CONCURRENT_PUTPATH).
    ExemptInfra,
    /// E4/E7 — worker `TimedOut` or controller `DeadlineExceeded`.
    Timeout,
    /// E3 — one of the seven permanent failure statuses.
    Permanent,
    /// A dependent swept to `DependencyFailed` by an ancestor's
    /// terminal failure (no execution of its own).
    Cascade,
    /// E8 — the scheduler-side backstop timer fired for a Running
    /// build with no report.
    Backstop,
    /// E5 — stream disconnect / heartbeat timeout / force-drain
    /// released the execution; classification not yet established
    /// (first installment of a two-installment attempt).
    Disconnected,
    /// A `disconnected` attempt whose classifying report never
    /// arrived: established by the correlation-TTL sweep (or the
    /// backstop) as an unreported executor crash.
    ExecutorCrash,
    /// E9 — dispatch-time fleet-exhaust verdict marker (not a
    /// charge; the fold treats it as a no-op event).
    FleetExhaust,
    /// Reset row: `dag::merge` resubmit reset of a retriable
    /// terminal node (carries the new `resubmit_cycle`).
    ResubmitReset,
    /// Reset row: cache-hit retry-state reset (output turned up
    /// in the store / re-probe found it substitutable).
    CacheHitClear,
    /// Reset row: admin `ClearPoison` or the poison-TTL expiry.
    PoisonCleared,
    /// Substitution-replacement: a materialization attempt confirmed a
    /// live-wanted path absent upstream after the full per-path retry
    /// ladder. A routing verdict, never a retry charge — invisible to
    /// every build budget (the kind partition, design §2.5).
    MaterializationUnobtainable,
    /// Substitution-replacement: a materialization attempt hit
    /// infrastructure failure (upstream 5xx/timeout/store-internal/
    /// no-tenant-context) or its executing replica crashed
    /// (establishment-written). Counts toward the materialization
    /// budget and toward NOTHING else — invisible to every build
    /// budget (the kind partition, design §2.5).
    MaterializationInfra,
    /// Substitution-replacement (migration 085): the materialization
    /// lane's reset row, written at job creation — one fresh budget
    /// window per job. Cuts the materialization-lane suffix exactly as
    /// a build reset cuts the build lane's (the cut predicate is
    /// `(kind, event_kind)`; this class is row DATA, never the cut).
    /// Invisible to every build budget (the kind partition).
    MaterializationReset,
    /// bug_408 (migration 088): an infrastructure failure the builder
    /// stamped `BuildResult.store_degraded` — the FUSE breaker was
    /// open at completion or tripped during the build. Attributable to
    /// the STORE, not the build or the node: the fold treats these
    /// rows as pure pacing (`sched.retry.store-degraded-uncharged`) —
    /// no count budget, no exclusion key, never poison; only
    /// `backoff_until` advances, from the consecutive run. There is
    /// deliberately NO `AttemptEvent` for this class: the charging
    /// alphabet cannot represent it, so a future `apply()` arm cannot
    /// charge it by accident.
    StoreDegraded,
}

/// Which party observed/reported the event behind an attempt-ledger
/// row (`drv_attempts.reporting_party`, migration 068) — the kernel-side
/// mirror of the scheduler's `ReportingParty` db enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportingParty {
    /// Worker `CompletionReport`.
    Worker,
    /// Controller `ReportExecutorTermination`.
    Controller,
    /// Scheduler-side observation (disconnect, backstop, sweep,
    /// dispatch-time verdict, TTL expiry).
    Scheduler,
    /// Admin RPC (ClearPoison).
    Admin,
}

/// Which work class an attempt row belongs to (substitution-replacement
/// campaign, design §2.5). The fold partitions on this and ONLY this
/// (never on outcome class — the partition must be total over every
/// channel that can produce a row): [`decide`] skips
/// materialization-kind rows entirely, and [`materialization_decide`]
/// folds only them.
///
/// The kernel-side mirror of `drv_executions.attempt_kind` (migration
/// 078): the scheduler's fold-input assembly joins the column onto each
/// ledger row; rows without an execution are `Build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttemptKind {
    /// A from-source build attempt (the as-built work class; the
    /// default for every row that predates the kind column).
    #[default]
    Build,
    /// A store-executed materialization attempt (fetch + verify of a
    /// derivation's wanted outputs from upstream caches).
    Materialization,
}

/// The decision-relevant projection of one `drv_attempts` row, in ledger
/// order. [`decide`] folds a suffix of these; the scheduler's
/// `retry_policy::decide()` shim builds them from `AttemptRecord`s
/// (dropping the fields the fold ignores and converting the f64 epoch
/// timestamp to the abstract clock at the boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow<Id> {
    /// Attempt event or reset event.
    pub event_kind: AttemptEventKind,
    /// Outcome classification (the [`classify`] alphabet).
    pub outcome_class: OutcomeClass,
    /// The attempt's exclusion/budget identity, when one was recorded
    /// on the row: the controller-authoritative source node (decision
    /// P12). `None` (an unbound attempt) charges flat counters only and
    /// never contributes an exclusion key.
    pub executor: Option<Id>,
    /// Who observed the event.
    pub reporting_party: ReportingParty,
    /// `FloorOutcome::promoted` at append time.
    pub floor_promoted: bool,
    /// `FloorOutcome::at_cap` at append time.
    pub floor_at_cap: bool,
    /// Resubmit cycle index this row belongs to (reset rows carry the
    /// new cycle).
    pub resubmit_cycle: i32,
    /// When the event occurred, on the abstract clock (epoch seconds).
    pub at: AbsTime,
    /// Work class (joined from `drv_executions.attempt_kind` at
    /// fold-input assembly; rows without an execution are `Build`).
    /// The kind partition keys on this and only this — see [`decide`]'s
    /// skip arm and [`materialization_decide`].
    pub kind: AttemptKind,
}

// ---------------------------------------------------------------------------
// Phase-1b decision surface: decide() / classify() / placeable()
//
// The design's frozen §5a-2 contract, layered on the fold above. The nine
// entry points in rio-scheduler are callers of these three functions
// (T-1b.2 onward, through the retry_policy shim); the fold core
// (`reference_fold` / `apply`) stays the executable spec and is not
// changed by the collapse.
// ---------------------------------------------------------------------------

/// The decision-surface output for one appending-transaction read: the
/// budget verdict plus the derived views the call sites consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision<Id> {
    /// The budget verdict as of the end of the history.
    pub verdict: Verdict,
    /// The per-executor exclusion set (the fold's `failed_builders`).
    /// E1's fleet-exhaust arm and the E9 dispatch backstop intersect it
    /// with the live eligible fleet via [`placeable`]; the spawn-intent
    /// exclusion consumes the same set through the fold-refreshed cached
    /// view (`RetryState::failed_builders`).
    pub exclusion: IdSet<Id>,
    /// The deterministic backoff deadline (no jitter — the dispatch site
    /// applies the production jitter exactly as today).
    pub backoff_until: Option<AbsTime>,
    /// The full fold-derived counter view.
    pub counters: Counters<Id>,
}

// ---------------------------------------------------------------------------
// decide()'s contract clauses, as named predicates
//
// The four `#[kani::ensures]` clauses on `decide()` are thin closures over
// these predicates, and `check_decide_contract` asserts the same predicates
// directly on the result of a plain call — one source of truth per clause,
// two consumers. The harness deliberately does NOT use
// `#[kani::proof_for_contract(decide)]`: kani's contract-instrumented
// wrapper around a fold of this size did not converge inside the
// merge-gate budget, while asserting the identical clauses over the
// identical domain in a plain proof does. The attributes stay on the
// function so the contract remains stated where it belongs (and remains
// available to a future `stub_verified` caller, which would have to bring
// its own `proof_for_contract` harness back with it).
// ---------------------------------------------------------------------------

/// Clause 1 of [`decide`]'s contract: the verdict partition is consistent
/// with the counters it was computed from — each terminal verdict names a
/// budget that really is at its bound, the TTL downgrade only fires on a
/// stamped expired poison, and the fleet-exhaust reason is unreachable
/// from `decide()` (placement is [`placeable`]'s job).
#[cfg(kani)]
fn decide_verdict_partition_consistent<Id: Ord>(
    d: &Decision<Id>,
    budget: &Budget,
    now: AbsTime,
) -> bool {
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
}

/// Clause 2 of [`decide`]'s contract: a Requeue verdict never exceeds a
/// budget cap — the per-cycle/infra/timeout caps hold over every
/// history, and the exempt cap holds over every history whose last
/// event is exempt-charging.
///
/// "Last event" means the last BUILD-kind row: the kind partition
/// (design §2.5) makes materialization-kind rows invisible to the build
/// fold, so the contract is stated over the build-kind view of history.
#[cfg(kani)]
fn decide_requeue_within_caps<Id: Ord>(
    d: &Decision<Id>,
    history: &[LedgerRow<Id>],
    budget: &Budget,
) -> bool {
    let last_is_exempt_charge = history
        .iter()
        .rev()
        .find(|r| r.kind == AttemptKind::Build)
        .is_some_and(|r| {
            r.event_kind == AttemptEventKind::Attempt
                && ((r.reporting_party == ReportingParty::Worker
                    && r.outcome_class == OutcomeClass::ExemptInfra)
                    || (r.reporting_party != ReportingParty::Worker
                        && !r.floor_at_cap
                        && (r.outcome_class == OutcomeClass::ExemptInfra
                            || (r.outcome_class == OutcomeClass::Infra && r.floor_promoted))))
        });
    d.counters.count <= budget.max_retries
        && d.counters.infra_count <= budget.max_infra_retries
        && d.counters.timeout_count <= budget.max_timeout_retries
        && (!matches!(d.verdict, Verdict::Requeue)
            || !last_is_exempt_charge
            || d.counters.exempt_infra_count < budget.max_exempt_infra_retries)
}

/// Clause 3 of [`decide`]'s contract: the exclusion set contains the
/// executor of every charged threshold attempt after the last reset row.
///
/// Both "reset row" and "charged attempt" range over BUILD-kind rows
/// only: the kind partition (design §2.5) keeps materialization-kind
/// rows out of the build fold, so a materialization row neither cuts
/// the window nor demands exclusion coverage.
#[cfg(kani)]
fn decide_exclusion_covers_charged_attempts<Id: Ord>(
    d: &Decision<Id>,
    history: &[LedgerRow<Id>],
) -> bool {
    let last_reset = history
        .iter()
        .rposition(|r| r.kind == AttemptKind::Build && r.event_kind == AttemptEventKind::Reset);
    let start = last_reset.map_or(0, |i| i + 1);
    history[start..]
        .iter()
        .filter(|r| r.kind == AttemptKind::Build)
        .all(|r| {
            let charges_threshold = r.event_kind == AttemptEventKind::Attempt
                && matches!(
                    r.outcome_class,
                    OutcomeClass::Transient
                        | OutcomeClass::Permanent
                        | OutcomeClass::Backstop
                        | OutcomeClass::ExecutorCrash
                );
            !charges_threshold || r.executor.as_ref().is_none_or(|e| d.exclusion.contains(e))
        })
}

/// Phase-1b decision function: fold a derivation's attempt-ledger suffix
/// into the budget verdict and the derived counter/exclusion views.
///
/// `history` is the post-reset suffix in ledger order (what
/// `load_attempt_suffix` returns, or the in-memory mirror of it),
/// INCLUDING the row the calling site just appended — the verdict is the
/// disposition produced by the last decision-bearing event. `now` is
/// epoch seconds (the same clock as the rows' `at`) and is consulted
/// only for the poison-TTL downgrade.
///
/// The fleet-exhaust arm is deliberately NOT part of this fold: the
/// eligible fleet is not history, so the in-history check is evaluated
/// against an empty fleet (never exhausted) and the call sites consume
/// [`Decision::exclusion`] through [`placeable`] instead.
///
/// **The kind partition (substitution-replacement, design §2.5):**
/// materialization-kind rows are invisible to this fold — every part of
/// it, including the resubmit-cycle seed read. They never charge a
/// build budget, never enter the poison thresholds, never contribute an
/// exclusion key, and never reset anything. Their own budget is
/// [`materialization_decide`]'s job. The
/// `materialization_rows_invisible_to_build_decision` unit test and the
/// `check_materialization_rows_invisible_to_build_decision` CBMC
/// harness pin this as an algebraic property: for any history,
/// `decide(history) == decide(build-kind rows of history)`.
///
/// This is the design's frozen §5a-2 three-argument decision surface.
/// (A fourth, transitional `legacy_seed` argument — the decision-P5
/// mixed-era floor read from the `derivations` mirror columns — existed
/// while pre-ledger failure histories could still be live; migration
/// 073 dropped the columns and the seed machinery was retired with
/// them.)
//
// ── Kani contracts ───────────────────────────────────────────────────
// No requires clause: the contract holds over the full input domain.
// Counter arithmetic cannot overflow at any reachable suffix length
// (every per-event charge is +1 onto a u32 and the clock arithmetic is
// saturating), so the harness bound on history length is a solver
// budget, not a soundness precondition. The three ensures clauses are,
// in order: the verdict partition is consistent with the counters it
// was computed from (each terminal verdict names a budget that really
// is at its bound, the TTL downgrade only fires on a stamped expired
// poison, and the fleet-exhaust reason is unreachable from decide() —
// placement is placeable()'s job); a Requeue verdict never exceeds a
// budget cap (the per-cycle/infra/timeout caps hold over every history,
// the exempt cap over every history whose last event is exempt-charging
// — the global form additionally needs the writer discipline that
// poisoned nodes get no further attempt rows, which is upstream of the
// fold); and the exclusion set contains the executor of every charged
// threshold attempt after the last reset row. The clause bodies are the
// `decide_*` predicate
// functions above (one source of truth); `check_decide_contract` in
// `#[cfg(kani)] mod proofs` asserts those same predicates on the result
// of a plain call rather than going through
// `#[kani::proof_for_contract]`, whose contract-instrumented wrapper
// around a fold this size does not converge inside the merge-gate
// budget. The two-call determinism property is the
// `check_decide_deterministic` harness.
#[cfg_attr(
    kani,
    kani::ensures(|d: &Decision<Id>| decide_verdict_partition_consistent(d, budget, now))
)]
#[cfg_attr(
    kani,
    kani::ensures(|d: &Decision<Id>| decide_requeue_within_caps(d, history, budget))
)]
#[cfg_attr(
    kani,
    kani::ensures(|d: &Decision<Id>| decide_exclusion_covers_charged_attempts(d, history))
)]
pub fn decide<Id: Ord + Clone>(
    history: &[LedgerRow<Id>],
    budget: &Budget,
    now: AbsTime,
) -> Decision<Id> {
    let mut initial = Counters::default();
    // A suffix that starts at a resubmit-reset row carries the new cycle
    // index on the row itself; seed the pre-fold counter so the reset
    // arm's `prior + 1` reproduces it (the loader cuts the suffix at the
    // most recent reset, so prior cycles are not otherwise visible).
    // "Starts at" means the first BUILD-kind row: the kind partition
    // makes materialization rows invisible to every part of this fold,
    // including this seed read.
    if let Some(first) = history.iter().find(|r| r.kind == AttemptKind::Build)
        && first.event_kind == AttemptEventKind::Reset
        && first.outcome_class == OutcomeClass::ResubmitReset
    {
        initial.resubmit_cycles = u32::try_from(first.resubmit_cycle)
            .unwrap_or(0)
            .saturating_sub(1);
    }

    // Fold the rows directly — no intermediate event buffer. The
    // in-history fleet-exhaust arm is evaluated against an empty fleet
    // (never exhausted): the eligible fleet is not history, and the call
    // sites consume the exclusion set through `placeable()` instead.
    let fleet = FleetView::default();
    let mut counters = initial;
    let mut verdict = Verdict::Requeue;
    // r[impl sched.retry.store-degraded-uncharged+2]
    // bug_408: the consecutive run of store-degraded rows. Pure
    // pacing — drives ONLY the backoff curve; reset by any other
    // folded event. Fold-local by design: not one of the ten
    // RetryState counters, never persisted.
    let mut store_degraded_run: u32 = 0;
    for row in history {
        // The kind partition (design §2.5): materialization-kind rows
        // are invisible to every build budget — they never charge
        // transient/infra/timeout caps, never enter the poison
        // thresholds, never contribute an exclusion key, and never
        // reset anything. They are folded by `materialization_decide`
        // instead.
        if row.kind == AttemptKind::Materialization {
            continue;
        }
        // bug_408: store-degraded rows are pacing, not charges. No
        // `AttemptEvent` exists for the class (`row_to_event` answers
        // `None`), so `apply()` structurally cannot charge it; the
        // verdict stays whatever the charged history decided, and only
        // the backoff advances — wait out the outage at the curve's
        // cap (`sched.retry.attempts-bounded+4`'s pacing carve-out).
        if row.event_kind == AttemptEventKind::Attempt
            && row.outcome_class == OutcomeClass::StoreDegraded
        {
            let until = row
                .at
                .saturating_add(budget.backoff_secs(store_degraded_run));
            counters.backoff_until = Some(match counters.backoff_until {
                Some(b) if b > until => b,
                _ => until,
            });
            store_degraded_run = store_degraded_run.saturating_add(1);
            continue;
        }
        if let Some(ev) = row_to_event(row) {
            store_degraded_run = 0;
            verdict = apply(&mut counters, &ev, budget, &fleet);
        }
    }
    let verdict = ttl_downgrade(&counters, verdict, now, budget);

    Decision {
        verdict,
        exclusion: counters.failed_builders.clone(),
        backoff_until: counters.backoff_until,
        counters,
    }
}

/// Map one ledger row onto the fold's event alphabet. Returns `None`
/// for rows that are deliberately no-ops for the fold: the per-dependent
/// `cascade` rows (the trigger's own poison row carries the charge) and
/// the E9 `fleet_exhaust` verdict marker (the placement verdict is
/// re-derived from the exclusion set and the live fleet by [`placeable`],
/// never folded from history).
fn row_to_event<Id: Clone>(row: &LedgerRow<Id>) -> Option<AttemptEvent<Id>> {
    let at = row.at;
    let executor = row.executor.clone();
    if row.event_kind == AttemptEventKind::Reset {
        return match row.outcome_class {
            OutcomeClass::ResubmitReset => Some(AttemptEvent::ResubmitReset { at }),
            OutcomeClass::CacheHitClear => Some(AttemptEvent::CacheHitClear { at }),
            OutcomeClass::PoisonCleared => Some(AttemptEvent::PoisonCleared { at }),
            // A reset row never carries an attempt class (writer
            // discipline + the migration's CHECK); fold a malformed one
            // as a no-op rather than guess.
            _ => None,
        };
    }
    let worker_reported = row.reporting_party == ReportingParty::Worker;
    match row.outcome_class {
        OutcomeClass::Transient => Some(AttemptEvent::Transient { at, executor }),
        OutcomeClass::Infra | OutcomeClass::ExemptInfra => {
            let exempt = row.outcome_class == OutcomeClass::ExemptInfra;
            if worker_reported {
                // E2 — the worker-reported arm, including its exempt
                // fall-through to the stale-window reset.
                Some(AttemptEvent::Infra {
                    at,
                    executor,
                    exempt,
                    at_cap: row.floor_at_cap,
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
                    promoted: exempt || row.floor_promoted,
                    at_cap: row.floor_at_cap,
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
        // bug_408: pacing class — handled by `decide()`'s own
        // backoff-only arm BEFORE this map runs; answering `None` here
        // means any other consumer of the event alphabet (the
        // worker-abort run counter, the divergence oracles) sees
        // store-degraded rows as the no-ops they are. There is no
        // `AttemptEvent` for the class: unrepresentable, not merely
        // unhandled.
        OutcomeClass::StoreDegraded => None,
        // Reset classes only ever ride on `event_kind = 'reset'` rows
        // (handled above); an attempt-kind row carrying one is malformed
        // — fold it as a no-op rather than guess.
        OutcomeClass::ResubmitReset | OutcomeClass::CacheHitClear | OutcomeClass::PoisonCleared => {
            None
        }
        // Substitution-replacement (PD-16): a materialization-class row
        // should never reach the build fold once the kind partition
        // (decide()'s materialization-kind skip arm) exists; this arm is
        // defense-in-depth ONLY — the partition is by attempt kind,
        // never by class (design §2.5). Folding a malformed
        // pre-partition row as a no-op follows the same precedent as
        // the malformed reset-class arms above. Nothing constructs
        // these classes yet (dormant until the materialization flags
        // enable the consumption/establishment writers).
        OutcomeClass::MaterializationUnobtainable
        | OutcomeClass::MaterializationInfra
        | OutcomeClass::MaterializationReset => None,
    }
}

// ---------------------------------------------------------------------------
// The materialization budget (substitution-replacement Phase A, design §2.5)
//
// The other half of the kind partition: `decide()` skips
// materialization-kind rows; this fold sees ONLY them. Dormant in Phase A
// (no scheduler call site constructs materialization rows until the
// flag-gated Wave-3 wiring); the function ships now so the partition is a
// complete, provable algebra rather than half of one.
// ---------------------------------------------------------------------------

/// The materialization-budget verdict (design §2.5): the disposition of
/// one derivation's materialization-kind ledger suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializationVerdict {
    /// Under budget: the job may be (re-)claimed.
    Claimable,
    /// Budget exhausted: park with backoff (never a fail-fast, never a
    /// poison — B3 "unknown never demotes"; the park is re-evaluated by
    /// housekeeping).
    Park,
}

/// Fold the materialization-kind rows of one derivation's ledger suffix
/// into the materialization-budget verdict.
///
/// Counts `MaterializationInfra` rows — worker-reported AND
/// establishment-written (OQ1 amendment 1: both channels charge the
/// same budget) — since the last materialization-kind reset row.
/// `MaterializationUnobtainable` rows are routing verdicts, not
/// retries: they are never counted. Build-kind rows are invisible here
/// (the partition is two-sided): they neither charge this budget nor
/// cut its reset window — pinned by the
/// `build_rows_invisible_to_materialization_decision` test.
///
/// `Park` at `count >= max_materialization_attempts`; never a poison,
/// never a fail-fast (the InfraFailure park-not-fail posture,
/// sched.materialize.routing).
pub fn materialization_decide<Id: Ord + Clone>(
    rows: &[LedgerRow<Id>],
    max_materialization_attempts: u32,
) -> MaterializationVerdict {
    if materialization_counters(rows).infra_since_reset >= max_materialization_attempts {
        MaterializationVerdict::Park
    } else {
        MaterializationVerdict::Claimable
    }
}

/// The materialization lane's windowed counters — THE single counter
/// for every budget/one-shot/strictness consumer (merged_bug_020: the
/// scheduler's flat per-class history counts are deleted in favor of
/// this fold). All three counts share ONE window: the suffix after the
/// last materialization-kind reset row (same cut as
/// [`materialization_decide`]; a 085_materialization_reset_class job-creation reset
/// re-zeros all three at once). Build-kind rows neither count nor cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MatCounters {
    /// `MaterializationInfra` rows in the window — worker-reported AND
    /// establishment-written (OQ1 amendment 1: both channels charge
    /// the same budget). The park predicate's count.
    pub infra_since_reset: u32,
    /// The worker-reported subset of `infra_since_reset` (the Item-T
    /// `conversion_requires_worker_charge` recount).
    pub worker_infra_since_reset: u32,
    /// `MaterializationUnobtainable` rows in the window (routing
    /// verdicts — the arm-0 one-shot discriminator, never a retry
    /// charge).
    pub unobtainable_since_reset: u32,
}

/// Fold one derivation's ledger rows into [`MatCounters`]. The window
/// is the materialization lane's reset cut; see [`MatCounters`].
pub fn materialization_counters<Id>(rows: &[LedgerRow<Id>]) -> MatCounters {
    let mut c = MatCounters::default();
    for r in rows
        .iter()
        // Same reset-cut discipline as the build fold, applied to the
        // materialization-kind view: count after the last
        // materialization-kind reset row. Build-kind rows (reset or
        // not) never cut this window.
        .rev()
        .take_while(|r| {
            !(r.kind == AttemptKind::Materialization && r.event_kind == AttemptEventKind::Reset)
        })
        .filter(|r| r.kind == AttemptKind::Materialization)
    {
        match r.outcome_class {
            OutcomeClass::MaterializationInfra => {
                c.infra_since_reset += 1;
                if r.reporting_party == ReportingParty::Worker {
                    c.worker_infra_since_reset += 1;
                }
            }
            OutcomeClass::MaterializationUnobtainable => c.unobtainable_since_reset += 1,
            _ => {}
        }
    }
    c
}

// ---------------------------------------------------------------------------
// Ledger GC sweep eligibility (sched.db.attempts-gc)
//
// The pure half of the drv_attempts retention sweep: the scheduler's
// SQL deletes the suffix complement (attempt-kind rows strictly before
// the last reset row, past the retention horizon, with no active
// assignment), and these functions state — and the proofs machine-check
// — that such a deletion leaves the decision suffix element-wise
// unchanged, hence `decide()` and `materialization_decide()`
// bit-identical. The theorem is deliberately loader-composed
// (suffix(sweep(L)) == suffix(L)) rather than raw fold equality over
// whole histories: a whole-history fold is NOT invariant (a pre-reset
// Transient's `backoff_until` survives a CacheHitClear in a raw fold),
// but production never folds whole histories — both suffix loaders cut
// at the last reset row, and that cut is what the sweep preserves.
// ---------------------------------------------------------------------------

/// The retention horizon for the attempt-ledger GC sweep, in seconds:
/// the largest decision window any fold consumer can look back across,
/// `max(retention_floor, infra_retry_window, poison_ttl)`.
///
/// The scheduler passes its `LEDGER_RETENTION_FLOOR` (24 h) as
/// `retention_floor_secs`; taking the floor as an argument keeps this
/// crate scheduler-agnostic while making the floor genuinely consulted
/// by the sweep. The `infra_retry_window_secs` term honors the floor
/// doc's "re-check against the configured value" clause: an
/// operator-widened window > 24 h widens the horizon with it. The
/// `poison_ttl_secs` term is currently dominated by the floor (the
/// scheduler's compile-time guard asserts floor >= POISON_TTL); it
/// binds independently only if the TTL ever becomes configurable or
/// outgrows the floor.
// r[impl sched.db.attempts-gc]
pub fn sweep_horizon_secs(budget: &Budget, retention_floor_secs: u64) -> u64 {
    retention_floor_secs
        .max(budget.infra_retry_window_secs)
        .max(budget.poison_ttl_secs)
}

/// Index where the decision suffix of ONE LANE of `rows` begins: the
/// position of the LAST row with `kind == lane && event_kind == Reset`,
/// or 0 when the lane has no reset row (the lane's whole history is
/// its suffix). The kernel mirror of the per-lane SQL cut
/// `(recorded_at, attempt_id) >= (last_lane_reset.recorded_at,
/// last_lane_reset.attempt_id)` — slice order here corresponds to
/// `(recorded_at, attempt_id)` order there, and the lane suffix
/// INCLUDES the lane's own reset row, exactly as both loaders return
/// it. Pinned to the SQL by the cross-layer DB test
/// `test_suffix_cut_matches_kernel_ledger_suffix_start` in
/// rio-scheduler.
// r[impl sched.db.attempts-gc]
pub fn ledger_suffix_start<Id>(rows: &[LedgerRow<Id>], lane: AttemptKind) -> usize {
    rows.iter()
        .rposition(|r| r.kind == lane && r.event_kind == AttemptEventKind::Reset)
        .unwrap_or(0)
}

/// Whether `rows[index]` is part of the loaded view: at-or-after the
/// suffix cut of ITS OWN lane (migration 084: the lane is a column on
/// every row, resets included — a build reset cuts only the build
/// lane, a materialization reset only the materialization lane). The
/// loaders' WHERE clause transcribes exactly this predicate (per-lane
/// LATERALs + a kind-keyed CASE), so the loaded view is the
/// order-preserving filter of the full history under this function —
/// the shape every sweep theorem quantifies over.
// r[impl sched.db.attempts-gc]
pub fn row_survives_load<Id>(rows: &[LedgerRow<Id>], index: usize) -> bool {
    index >= ledger_suffix_start(rows, rows[index].kind)
}

/// Sweep eligibility of `rows[index]` under the live-derivation arm:
/// an attempt-kind row strictly before ITS OWN lane's suffix cut,
/// older than the horizon. Reset rows are NEVER eligible (keeping
/// every reset row makes "the cut never moves backward" structural),
/// and a lane with no reset row has no cut — none of its rows are
/// eligible, that lane's whole history is its live suffix.
///
/// The age conjunct here models time on the kernel's single abstract
/// clock while the SQL transcription ages on PG-assigned `recorded_at`;
/// this is sound because the proofs quantify over the STRUCTURAL
/// predicate only (any age conjunct merely shrinks the deleted set, so
/// every age implementation — any clock, any skew — is a special case
/// of the proven mask domain). The scheduler-side E4 conjunct (no
/// active assignment for the row's exec_id) is likewise a
/// deletion-set-shrinking refinement, owned by the SQL and its DB
/// tests: it protects the report-idempotency probes, not the fold.
// r[impl sched.db.attempts-gc]
pub fn sweep_eligible<Id>(
    rows: &[LedgerRow<Id>],
    index: usize,
    now: AbsTime,
    horizon_secs: u64,
) -> bool {
    rows[index].event_kind == AttemptEventKind::Attempt
        && index < ledger_suffix_start(rows, rows[index].kind)
        && now.saturating_sub(rows[index].at) > horizon_secs
}

/// How a worker-reported abort (`BuildResultStatus::Cancelled` for a
/// still-wanted open build attempt — the AD5 SIGTERM-abort report) is
/// admitted, given the attempt history.
// r[impl sched.attempt.worker-abort-bounded]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerAbortAdmission {
    /// Below the bound: close the attempt charge-free and requeue (the
    /// as-built AD5 posture).
    Uncharged,
    /// The trailing run of build-lane worker-abort closures has reached
    /// the bound: the report falls through to the charged
    /// infrastructure classification (the unsolicited-Cancelled arm),
    /// consuming the attempt WITH budget.
    ChargedFallthrough,
}

/// How many CONSECUTIVE worker-abort closures are admitted charge-free
/// before the next one is charged. Platform terminations (preemption,
/// scale-down, controller deletes) legitimately produce short runs;
/// a worker looping `pull → Cancelled` produces an unbounded one —
/// bug_279's uncharged-requeue mint. Three free closes absorb any
/// plausible disruption burst while keeping the loop finite.
pub const WORKER_ABORT_FREE_CLOSES: u32 = 3;

/// Admit one worker-abort report against the attempt history: count
/// the TRAILING run of build-lane free-close rows
/// (`Attempt ∧ Disconnected ∧ Worker` — exactly the row the uncharged
/// close appends; pull-mode's only worker-party `Disconnected` writer)
/// and admit `Uncharged` only while the run is strictly below `bound`.
///
/// Lane discipline (the kind partition): MATERIALIZATION-lane rows
/// neither extend nor break the run — they are skipped exactly as
/// [`decide`]'s fold skips them, so a store replica's interleaved
/// charges cannot launder a worker's abort loop back under the bound.
/// Any OTHER build-lane row (a charged classification, a controller
/// row, a reset) breaks the run: the loop signature is consecutive
/// worker aborts with nothing else happening to the lane.
// r[impl sched.attempt.worker-abort-bounded]
pub fn admit_worker_abort<Id>(rows: &[LedgerRow<Id>], bound: u32) -> WorkerAbortAdmission {
    let mut run: u32 = 0;
    let mut i = rows.len();
    while i > 0 {
        i -= 1;
        let r = &rows[i];
        if r.kind != AttemptKind::Build {
            continue;
        }
        let is_free_close = r.event_kind == AttemptEventKind::Attempt
            && r.outcome_class == OutcomeClass::Disconnected
            && r.reporting_party == ReportingParty::Worker;
        if !is_free_close {
            break;
        }
        run += 1;
        if run >= bound {
            return WorkerAbortAdmission::ChargedFallthrough;
        }
    }
    WorkerAbortAdmission::Uncharged
}

/// How many CONSECUTIVE store-degraded reports are admitted into the
/// uncharged pacing class before the next one is charged
/// (merged_bug_032, bughunt-2 slot 3). The flag is WORKER-SUPPLIED
/// evidence: without a bound, a worker stamping every report
/// `store_degraded=true` mints unbounded uncharged requeues — bug_279's
/// shape reproduced inside bug_408's own fix. Twelve free runs ≈ 35min
/// of minimum paced outage at the default backoff curve (and exceeds
/// the landed 11-report pacing test, whose assertions survive); a real
/// store outage longer than that falls through into the counted infra
/// budget and becomes operator-visible poison ~10 attempts later —
/// never instant. Signed: bughunt-2 §5-S Q5 (2026-06-04).
// r[impl sched.retry.store-degraded-uncharged+2]
pub const STORE_DEGRADED_FREE_RUN: u32 = 12;

/// The single source of truth for UNCHARGED outcome classes and their
/// consecutive-run bounds. Every class whose fold treatment is
/// charge-free pacing MUST appear here with a finite bound — the
/// `uncharged_classes_are_bounded_or_marked` disposition lint forces
/// every `OutcomeClass` variant (present and future) to declare which
/// bucket it lives in, so an unbounded uncharged class cannot be
/// introduced by review miss again.
pub const BOUNDED_UNCHARGED: &[(OutcomeClass, u32)] = &[
    (OutcomeClass::Disconnected, WORKER_ABORT_FREE_CLOSES),
    (OutcomeClass::StoreDegraded, STORE_DEGRADED_FREE_RUN),
];

/// Admit one store-degraded report against the attempt history: count
/// the TRAILING run of build-lane store-degraded pacing rows
/// (`Attempt ∧ StoreDegraded ∧ Worker` — exactly the row the uncharged
/// paced write appends) and admit `Uncharged` only while the run is
/// strictly below `bound`; at the bound the report falls through to
/// the CHARGED infra path (counted budget → operator-visible poison),
/// mirroring [`admit_worker_abort`]'s discipline.
///
/// Lane discipline (the kind partition): MATERIALIZATION-lane rows are
/// skipped exactly as [`decide`]'s fold skips them; any OTHER
/// build-lane row (a charged classification, a reset, a worker abort)
/// breaks the run — the unbounded-mint signature is consecutive
/// flagged reports with nothing else happening to the lane.
// r[impl sched.retry.store-degraded-uncharged+2]
pub fn admit_store_degraded<Id>(rows: &[LedgerRow<Id>], bound: u32) -> WorkerAbortAdmission {
    let mut run: u32 = 0;
    let mut i = rows.len();
    while i > 0 {
        i -= 1;
        let r = &rows[i];
        if r.kind != AttemptKind::Build {
            continue;
        }
        let is_paced = r.event_kind == AttemptEventKind::Attempt
            && r.outcome_class == OutcomeClass::StoreDegraded
            && r.reporting_party == ReportingParty::Worker;
        if !is_paced {
            break;
        }
        run += 1;
        if run >= bound {
            return WorkerAbortAdmission::ChargedFallthrough;
        }
    }
    WorkerAbortAdmission::Uncharged
}

/// Sweep eligibility of one `drv_executions` lifecycle ROW (not a
/// ledger row): the second deleter of the retention story
/// (`store.log.sweep-ownership` — the store's log TTL sweep no longer
/// touches these rows). A pure conjunction, deliberately STRONGER than
/// "not in the decision suffix":
///
/// - `terminal`: a non-terminal row may still receive its report; its
///   exec_id is a live idempotency key.
/// - `!has_active_assignment`: protects the report-idempotency probes
///   exactly as the ledger sweep's E4 conjunct does.
/// - `!referenced_by_ledger`: an exec row outlives EVERY `drv_attempts`
///   row that needs its kind — the kind-resolution `COALESCE` decay
///   (a referenced exec deleted ⇒ its attempts silently re-kind
///   `'build'`) is unreachable. The ledger GC bounds attempt-row
///   lifetime, so exec rows stay eventually collectable: parked >30 d
///   derivations keep their charge rows in the post-reset suffix,
///   those rows keep their exec rows, and the kind survives the park.
/// - `aged_out`: past `exec_retention_days` (the SQL twin binds the
///   configured value).
///
/// The SQL twin is `gc_exec_rows` in rio-scheduler `db/attempts.rs`;
/// its DB tests pin all four conjuncts against real rows.
// r[impl store.log.sweep-ownership]
pub fn exec_row_sweep_eligible(
    terminal: bool,
    has_active_assignment: bool,
    referenced_by_ledger: bool,
    aged_out: bool,
) -> bool {
    terminal && !has_active_assignment && !referenced_by_ledger && aged_out
}

/// The floor-bump outcome as [`classify`] consumes it — a leaf-local
/// mirror of the actor's `FloorOutcome` so this crate keeps no actor
/// dependency. `promoted` and `at_cap` are mutually exclusive; both are
/// false for non-resource events and for the cold-start no-intent case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FloorOutcomeView {
    /// The floor doubled — the attempt is a sizing signal, exempt from
    /// the non-exempt infra cap.
    pub promoted: bool,
    /// The relevant dimension was already at its ceiling.
    pub at_cap: bool,
}

/// One observed failure trigger, as the entry point sees it at append
/// time — the input vocabulary of [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedFailure<'a> {
    /// E1 — worker `CompletionReport{TransientFailure}` or `Unspecified`.
    WorkerTransient,
    /// E2 — worker `CompletionReport{InfrastructureFailure}`; the error
    /// message drives the CONCURRENT_PUTPATH half of the exemption.
    WorkerInfra {
        /// The worker-reported error message.
        error_msg: &'a str,
    },
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
    /// bug_408 — worker `CompletionReport{InfrastructureFailure}` with
    /// `BuildResult.store_degraded` set: the builder's FUSE breaker
    /// attributes the failure to a degraded store. Never consults the
    /// floor and never exempts — the class IS the disposition.
    WorkerStoreDegraded,
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
            if floor.promoted || contains_concurrent_putpath_marker(error_msg) {
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
        ObservedFailure::WorkerStoreDegraded => *c == OutcomeClass::StoreDegraded,
    }
}))]
pub fn classify(event: &ObservedFailure<'_>, floor: FloorOutcomeView) -> OutcomeClass {
    match event {
        ObservedFailure::WorkerTransient => OutcomeClass::Transient,
        ObservedFailure::WorkerInfra { error_msg } => {
            if floor.promoted || contains_concurrent_putpath_marker(error_msg) {
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
        // r[impl sched.retry.store-degraded-uncharged+2]
        ObservedFailure::WorkerStoreDegraded => OutcomeClass::StoreDegraded,
    }
}

/// The placement verdict for a derivation given its exclusion set and
/// the live eligible fleet — [`exhausts_fleet`]'s answer plus the
/// "is there anyone left to take it" discrimination the dispatch site
/// needs. Pure; the operator-facing exhaustion observability (warn! +
/// metric) stays at the call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// At least one eligible worker is not in the exclusion set.
    Placeable,
    /// The eligible fleet is non-empty and every member has already
    /// failed this derivation — the fleet-exhaust poison arm (E1/E9).
    FleetExhausted,
    /// No statically-eligible, non-draining worker is registered at all:
    /// defer, never poison (the empty-fleet clause of
    /// `sched.dispatch.fleet-exhaust+5` — an empty pool is a
    /// provisioning transient).
    NoEligibleWorkers,
}

// r[impl sched.dispatch.fleet-exhaust+5]
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
// empty-fleet clause of `sched.dispatch.fleet-exhaust+5`), exhaustion
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
pub fn placeable<Id: Ord>(excluded: &IdSet<Id>, eligible: &IdSet<Id>) -> Placement {
    if eligible.is_empty() {
        return Placement::NoEligibleWorkers;
    }
    if !excluded.is_empty() && eligible.is_subset(excluded) {
        Placement::FleetExhausted
    } else {
        Placement::Placeable
    }
}

#[cfg(test)]
mod tests {
    //! Crate-local smoke tests. The load-bearing behavioral battery for
    //! the fold and the decision surface (the hand-computed histories
    //! and the divergence reproducers) lives in
    //! `rio-scheduler/src/retry_policy.rs` and exercises these kernels
    //! through the production projection shim — that suite is the
    //! equivalence oracle for the extraction. The tests here only pin
    //! kernel-local concerns: the backoff curve, the placement/fleet
    //! partitions, that the generic identity parameter does not affect
    //! a verdict, and the proof-representation pins (the
    //! [`BoundedIdSet`]↔`BTreeSet` differential battery and the
    //! substring-predicate↔`str::contains` agreement).

    /// bughunt-2 slot 3 disposition lint (merged_bug_032's class
    /// close): EVERY `OutcomeClass` variant — present and future — must
    /// declare its charge disposition in the exhaustive no-wildcard
    /// match below; a new variant fails to compile until classified,
    /// and a class declared `BoundedUncharged` without (or with a
    /// mismatched) [`super::BOUNDED_UNCHARGED`] entry fails the
    /// asserts. bug_279 → merged_bug_032 (an uncharged class shipped
    /// without a bound, twice) cannot recur as a review miss.
    #[test]
    fn uncharged_classes_are_bounded_or_marked() {
        use super::OutcomeClass as C;

        #[derive(PartialEq)]
        enum Disposition {
            /// Charged into a counted budget (or counted-exempt).
            ChargedAlphabet,
            /// Lane reset marker — cuts a suffix, never a charge.
            ResetMarker,
            /// Routing/dispatch verdict marker — fold no-op.
            VerdictMarker,
            /// Uncharged pacing class — MUST carry a finite
            /// consecutive-run bound in `BOUNDED_UNCHARGED`.
            BoundedUncharged,
        }

        let disposition = |class: C| -> Disposition {
            match class {
                C::Transient
                | C::Infra
                | C::ExemptInfra
                | C::Timeout
                | C::Permanent
                | C::Cascade
                | C::Backstop
                | C::ExecutorCrash
                | C::MaterializationInfra => Disposition::ChargedAlphabet,
                C::ResubmitReset
                | C::CacheHitClear
                | C::PoisonCleared
                | C::MaterializationReset => Disposition::ResetMarker,
                C::FleetExhaust | C::MaterializationUnobtainable => Disposition::VerdictMarker,
                C::Disconnected | C::StoreDegraded => Disposition::BoundedUncharged,
            }
        };

        let all = [
            C::Transient,
            C::Infra,
            C::ExemptInfra,
            C::Timeout,
            C::Permanent,
            C::Cascade,
            C::Backstop,
            C::Disconnected,
            C::ExecutorCrash,
            C::FleetExhaust,
            C::ResubmitReset,
            C::CacheHitClear,
            C::PoisonCleared,
            C::MaterializationUnobtainable,
            C::MaterializationInfra,
            C::MaterializationReset,
            C::StoreDegraded,
        ];
        for class in all {
            let bounded = super::BOUNDED_UNCHARGED
                .iter()
                .any(|(c, bound)| *c == class && *bound >= 1);
            assert_eq!(
                disposition(class) == Disposition::BoundedUncharged,
                bounded,
                "{class:?}: BoundedUncharged classes and BOUNDED_UNCHARGED \
                 entries must match exactly (finite bound required)"
            );
        }
        assert_eq!(
            super::BOUNDED_UNCHARGED.len(),
            2,
            "every BOUNDED_UNCHARGED entry must be a classified variant"
        );
    }

    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn backoff_curve_is_exponential_and_capped() {
        let b = Budget::default();
        // base 5, mult 2, cap 300.
        assert_eq!(b.backoff_secs(0), 5);
        assert_eq!(b.backoff_secs(1), 10);
        assert_eq!(b.backoff_secs(2), 20);
        assert_eq!(b.backoff_secs(10), 300, "capped at backoff_max_secs");
    }

    /// `admit_worker_abort` decision table (bug_279): the trailing
    /// build-lane free-close run, the lane skip, and the run breakers.
    // r[verify sched.attempt.worker-abort-bounded]
    #[test]
    fn worker_abort_admission_table() {
        use WorkerAbortAdmission::{ChargedFallthrough, Uncharged};
        let free = |at: u64| LedgerRow::<u8> {
            event_kind: AttemptEventKind::Attempt,
            outcome_class: OutcomeClass::Disconnected,
            executor: None,
            reporting_party: ReportingParty::Worker,
            floor_promoted: false,
            floor_at_cap: false,
            resubmit_cycle: 0,
            at,
            kind: AttemptKind::Build,
        };
        let controller_disc = |at: u64| LedgerRow {
            reporting_party: ReportingParty::Controller,
            ..free(at)
        };
        let build_infra = |at: u64| LedgerRow {
            outcome_class: OutcomeClass::Infra,
            ..free(at)
        };
        let mat_infra = |at: u64| LedgerRow {
            outcome_class: OutcomeClass::MaterializationInfra,
            kind: AttemptKind::Materialization,
            ..free(at)
        };
        let bound = WORKER_ABORT_FREE_CLOSES;

        // Empty and short runs admit uncharged.
        assert_eq!(admit_worker_abort::<u8>(&[], bound), Uncharged);
        assert_eq!(admit_worker_abort(&[free(1), free(2)], bound), Uncharged);
        // At the bound: charged fall-through.
        assert_eq!(
            admit_worker_abort(&[free(1), free(2), free(3)], bound),
            ChargedFallthrough
        );
        // run-broken-by-controller-row: a trailing controller
        // Disconnected breaks the run (consecutive WORKER aborts only).
        assert_eq!(
            admit_worker_abort(&[free(1), free(2), free(3), controller_disc(4)], bound),
            Uncharged
        );
        // A charged build row breaks it too.
        assert_eq!(
            admit_worker_abort(&[free(1), free(2), build_infra(3), free(4)], bound),
            Uncharged
        );
        // mat-row-interleaved: materialization-lane rows neither extend
        // nor break the run — the partition holds at this gate exactly
        // as it does in decide()'s fold.
        assert_eq!(
            admit_worker_abort(
                &[free(1), mat_infra(2), free(3), mat_infra(4), free(5)],
                bound
            ),
            ChargedFallthrough
        );
        // Build reset breaks the run (a fresh cycle starts clean).
        let reset = LedgerRow {
            event_kind: AttemptEventKind::Reset,
            ..free(6)
        };
        assert_eq!(
            admit_worker_abort(&[free(1), free(2), reset, free(4)], bound),
            Uncharged
        );
    }

    #[test]
    fn placeable_partition_over_str_ids() {
        let excluded: BTreeSet<&str> = ["w1"].into_iter().collect();
        let eligible: BTreeSet<&str> = ["w1", "w2"].into_iter().collect();
        assert_eq!(placeable(&excluded, &eligible), Placement::Placeable);

        let all_failed: BTreeSet<&str> = ["w1", "w2"].into_iter().collect();
        assert_eq!(placeable(&all_failed, &eligible), Placement::FleetExhausted);

        let empty_fleet: BTreeSet<&str> = BTreeSet::new();
        assert_eq!(
            placeable(&all_failed, &empty_fleet),
            Placement::NoEligibleWorkers
        );
    }

    #[test]
    fn exhausts_fleet_requires_nonempty_exclusion_and_fleet() {
        let mut fleet = FleetView::<String>::default();
        let empty: BTreeSet<String> = BTreeSet::new();
        let failed: BTreeSet<String> = ["w1".to_string()].into_iter().collect();
        // Empty fleet never exhausts, empty exclusion never exhausts.
        assert!(!exhausts_fleet(&failed, &fleet));
        fleet.eligible.insert("w1".to_string());
        assert!(!exhausts_fleet(&empty, &fleet));
        assert!(exhausts_fleet(&failed, &fleet));
    }

    #[test]
    fn verdicts_do_not_depend_on_the_identity_type() {
        // The same two-distinct-executor transient history, folded once
        // with string identities and once with integer identities:
        // identical counters modulo the identity values, identical
        // verdict. (The kani harnesses rely on this to prove over a
        // small copy type what production runs over String.)
        let by_str = [
            AttemptEvent::Transient {
                at: 100,
                executor: Some("w1".to_string()),
            },
            AttemptEvent::Transient {
                at: 200,
                executor: Some("w2".to_string()),
            },
        ];
        let by_int = [
            AttemptEvent::Transient {
                at: 100,
                executor: Some(1u8),
            },
            AttemptEvent::Transient {
                at: 200,
                executor: Some(2u8),
            },
        ];
        let (cs, vs) = reference_fold(&by_str, 200, &Budget::default(), &FleetView::default());
        let (ci, vi) = reference_fold(&by_int, 200, &Budget::default(), &FleetView::default());
        assert_eq!(vs, vi);
        assert_eq!(cs.count, ci.count);
        assert_eq!(cs.failure_count, ci.failure_count);
        assert_eq!(cs.failed_builders.len(), ci.failed_builders.len());
        assert_eq!(cs.backoff_until, ci.backoff_until);
    }

    #[test]
    fn decide_empty_history_is_default_requeue() {
        let d = decide::<String>(&[], &Budget::default(), 0);
        assert_eq!(d.verdict, Verdict::Requeue);
        assert!(d.exclusion.is_empty());
        assert_eq!(d.backoff_until, None);
        assert_eq!(d.counters, Counters::default());
    }

    /// The proof-time set representation ([`BoundedIdSet`]) agrees with
    /// the production representation (`BTreeSet`) on every observable
    /// the kernel uses — `insert`'s newness verdict, `len`, `is_empty`,
    /// `contains`, and the element set yielded by `iter` — over every
    /// insert sequence of length ≤ 5 drawn from a capacity-sized domain
    /// (exhaustive), which covers every duplicate/ordering shape the
    /// bounded harness domains can produce.
    #[test]
    fn bounded_id_set_agrees_with_btreeset_exhaustively() {
        const DOMAIN: [u8; BOUNDED_ID_SET_CAPACITY] = [0, 1, 3, 7];
        for len in 0..=5u32 {
            let sequences = DOMAIN.len().pow(len);
            for sequence in 0..sequences {
                let mut code = sequence;
                let mut bounded = BoundedIdSet::new();
                let mut reference = BTreeSet::new();
                for _ in 0..len {
                    let value = DOMAIN[code % DOMAIN.len()];
                    code /= DOMAIN.len();
                    assert_eq!(bounded.insert(value), reference.insert(value));
                    assert_eq!(bounded.len(), reference.len());
                    assert_eq!(bounded.is_empty(), reference.is_empty());
                    for probe in DOMAIN {
                        assert_eq!(bounded.contains(&probe), reference.contains(&probe));
                    }
                    let mut yielded: Vec<u8> = bounded.iter().copied().collect();
                    yielded.sort_unstable();
                    let expected: Vec<u8> = reference.iter().copied().collect();
                    assert_eq!(yielded, expected, "iter() must yield exactly the members");
                }
            }
        }
    }

    /// `is_subset` (the surface [`exhausts_fleet`] / [`placeable`]
    /// consume) agrees with `BTreeSet::is_subset` over the
    /// empty/subset/equal/superset/disjoint/overlap shapes.
    #[test]
    fn bounded_id_set_is_subset_agrees_with_btreeset() {
        let pairs: [(&[u8], &[u8]); 8] = [
            (&[], &[]),
            (&[], &[1]),
            (&[1], &[]),
            (&[1], &[1, 2]),
            (&[1, 2], &[1, 2]),
            (&[1, 2, 3], &[1, 2]),
            (&[1, 4], &[2, 3]),
            (&[1, 2], &[2, 3]),
        ];
        for (left, right) in pairs {
            let bounded_left: BoundedIdSet<u8> = left.iter().copied().collect();
            let bounded_right: BoundedIdSet<u8> = right.iter().copied().collect();
            let reference_left: BTreeSet<u8> = left.iter().copied().collect();
            let reference_right: BTreeSet<u8> = right.iter().copied().collect();
            assert_eq!(
                bounded_left.is_subset(&bounded_right),
                reference_left.is_subset(&reference_right),
                "is_subset disagrees for {left:?} vs {right:?}"
            );
        }
    }

    /// `FromIterator`/`Extend` on the proof-time set dedup exactly like
    /// collecting into a `BTreeSet` (the conversion surface a caller
    /// building an [`IdSet`] generically would hit).
    #[test]
    fn bounded_id_set_collect_dedups_like_btreeset() {
        let values = [3u8, 1, 3, 0, 1];
        let bounded: BoundedIdSet<u8> = values.into_iter().collect();
        let reference: BTreeSet<u8> = values.into_iter().collect();
        assert_eq!(bounded.len(), reference.len());
        for v in &reference {
            assert!(bounded.contains(v));
        }
        let mut extended = bounded.clone();
        extended.extend([1u8, 9]);
        assert_eq!(extended.len(), reference.len() + 1);
        assert!(extended.contains(&9));
    }

    /// Inserting more distinct values than the capacity panics: the
    /// proof harnesses' domains stay below it, and the panic (a CBMC
    /// verification failure if ever reachable) is the tripwire that
    /// forces the capacity up rather than silently dropping elements.
    #[test]
    #[should_panic(expected = "BoundedIdSet capacity exceeded")]
    fn bounded_id_set_insert_panics_past_capacity() {
        let mut s = BoundedIdSet::new();
        for v in 0..=u8::try_from(BOUNDED_ID_SET_CAPACITY).expect("small capacity") {
            s.insert(v);
        }
    }

    // -----------------------------------------------------------------
    // The kind partition (substitution-replacement Phase A, design §2.5)
    // -----------------------------------------------------------------

    /// A build-kind ledger row for the partition battery.
    fn build_row(class: OutcomeClass, executor: Option<&str>, at: AbsTime) -> LedgerRow<String> {
        LedgerRow {
            event_kind: AttemptEventKind::Attempt,
            outcome_class: class,
            executor: executor.map(str::to_string),
            reporting_party: ReportingParty::Worker,
            floor_promoted: false,
            floor_at_cap: false,
            resubmit_cycle: 0,
            at,
            kind: AttemptKind::Build,
        }
    }

    /// A build-kind reset row (`event_kind = Reset`).
    fn reset_build_row(class: OutcomeClass, resubmit_cycle: i32, at: AbsTime) -> LedgerRow<String> {
        LedgerRow {
            event_kind: AttemptEventKind::Reset,
            reporting_party: ReportingParty::Scheduler,
            resubmit_cycle,
            ..build_row(class, None, at)
        }
    }

    /// A materialization-kind ledger row.
    fn mat_row(class: OutcomeClass, executor: Option<&str>, at: AbsTime) -> LedgerRow<String> {
        LedgerRow {
            kind: AttemptKind::Materialization,
            ..build_row(class, executor, at)
        }
    }

    /// materializationInvisibleToBuildBudgets, kernel half (design §2.5,
    /// review findings PP-4/BC-2): for ANY history, interleaving ANY
    /// number of materialization-kind rows anywhere in it never changes
    /// the build-budget decision (verdict, exclusion set, backoff,
    /// counters).
    #[test]
    fn materialization_rows_invisible_to_build_decision() {
        let budget = Budget::default();
        // Representative build history: same-worker transients, a worker
        // infra, an established crash, a resubmit reset, a fresh-cycle
        // transient.
        let build_history = vec![
            build_row(OutcomeClass::Transient, Some("w1"), 100),
            build_row(OutcomeClass::Transient, Some("w1"), 200),
            build_row(OutcomeClass::Infra, Some("w2"), 300),
            build_row(OutcomeClass::ExecutorCrash, Some("w3"), 400),
            reset_build_row(OutcomeClass::ResubmitReset, 1, 500),
            build_row(OutcomeClass::Transient, Some("w1"), 600),
        ];
        let baseline = decide(&build_history, &budget, 600);

        // Materialization rows of both classes, with/without identity.
        let mat_rows = [
            mat_row(OutcomeClass::MaterializationInfra, Some("store-0"), 50),
            mat_row(OutcomeClass::MaterializationUnobtainable, None, 250),
            mat_row(OutcomeClass::MaterializationInfra, Some("store-1"), 450),
            mat_row(OutcomeClass::MaterializationInfra, None, 700),
        ];

        // All four interleaved at spread positions.
        let mut interleaved = build_history.clone();
        interleaved.insert(0, mat_rows[0].clone());
        interleaved.insert(3, mat_rows[1].clone());
        interleaved.insert(6, mat_rows[2].clone());
        interleaved.push(mat_rows[3].clone());
        assert_eq!(
            decide(&interleaved, &budget, 600),
            baseline,
            "interleaved materialization rows must be invisible to the build fold"
        );

        // Every single-insertion position × every materialization row.
        for pos in 0..=build_history.len() {
            for m in &mat_rows {
                let mut h = build_history.clone();
                h.insert(pos, m.clone());
                assert_eq!(
                    decide(&h, &budget, 600),
                    baseline,
                    "a materialization row at position {pos} changed the build decision"
                );
            }
        }

        // The seed corner: a history whose FIRST build row is a
        // resubmit-reset (the cycle-seed read) — a materialization row
        // inserted ahead of it must not break the seed.
        let reset_first = vec![
            reset_build_row(OutcomeClass::ResubmitReset, 2, 100),
            build_row(OutcomeClass::Transient, Some("w1"), 200),
        ];
        let seed_baseline = decide(&reset_first, &budget, 300);
        let mut with_prefix = reset_first.clone();
        with_prefix.insert(0, mat_row(OutcomeClass::MaterializationInfra, None, 50));
        assert_eq!(
            decide(&with_prefix, &budget, 300),
            seed_baseline,
            "a materialization row ahead of the resubmit-reset seed row must not change the seed"
        );
    }

    /// The materialization budget: N materialization_infra rows since
    /// the last (materialization) reset → Park verdict at
    /// N >= max_attempts; unobtainable rows are NOT counted (they are
    /// routing verdicts, not retries).
    #[test]
    fn materialization_budget_counts_infra_only() {
        // 2 infra + 2 unobtainable, budget 3: claimable.
        let h = vec![
            mat_row(OutcomeClass::MaterializationInfra, Some("store-0"), 100),
            mat_row(
                OutcomeClass::MaterializationUnobtainable,
                Some("store-0"),
                200,
            ),
            mat_row(OutcomeClass::MaterializationInfra, Some("store-1"), 300),
            mat_row(
                OutcomeClass::MaterializationUnobtainable,
                Some("store-1"),
                400,
            ),
        ];
        assert_eq!(
            materialization_decide(&h, 3),
            MaterializationVerdict::Claimable,
            "unobtainable rows must not charge the materialization budget"
        );

        // A third infra row exhausts the budget: park.
        let mut h3 = h.clone();
        h3.push(mat_row(
            OutcomeClass::MaterializationInfra,
            Some("store-2"),
            500,
        ));
        assert_eq!(materialization_decide(&h3, 3), MaterializationVerdict::Park);

        // Empty history: claimable.
        assert_eq!(
            materialization_decide::<String>(&[], 3),
            MaterializationVerdict::Claimable
        );

        // Zero budget: parked immediately (never claimable).
        assert_eq!(materialization_decide(&h, 0), MaterializationVerdict::Park);
    }

    /// Crash-establishment rows (kind=Materialization,
    /// class=MaterializationInfra, party=Scheduler) count toward the
    /// materialization budget — OQ1 amendment 1's channel: the
    /// establishment sweep and the worker report charge the same budget.
    #[test]
    fn establishment_written_infra_rows_count_toward_materialization_budget() {
        // One worker-reported infra row.
        let worker = mat_row(OutcomeClass::MaterializationInfra, Some("store-0"), 100);
        // Two establishment-written rows: party=Scheduler, no identity
        // (the executing replica crashed without reporting).
        let mut crash_a = mat_row(OutcomeClass::MaterializationInfra, None, 200);
        crash_a.reporting_party = ReportingParty::Scheduler;
        let mut crash_b = crash_a.clone();
        crash_b.at = 300;

        let h = vec![worker, crash_a, crash_b];
        assert_eq!(
            materialization_decide(&h[..2], 3),
            MaterializationVerdict::Claimable,
            "two of three: still claimable"
        );
        assert_eq!(
            materialization_decide(&h, 3),
            MaterializationVerdict::Park,
            "establishment-written rows charge the same budget as worker-reported ones"
        );
    }

    /// Build-kind rows are invisible to the materialization budget (the
    /// partition is two-sided): they neither charge it nor cut its
    /// reset window — including build RESET rows.
    #[test]
    fn build_rows_invisible_to_materialization_decision() {
        let mat_history = vec![
            mat_row(OutcomeClass::MaterializationInfra, Some("store-0"), 100),
            mat_row(OutcomeClass::MaterializationInfra, Some("store-1"), 300),
        ];
        let baseline = materialization_decide(&mat_history, 2);
        assert_eq!(baseline, MaterializationVerdict::Park);

        // Build rows of every flavor — including reset rows, which must
        // NOT cut the materialization count.
        let build_rows = [
            build_row(OutcomeClass::Transient, Some("w1"), 150),
            build_row(OutcomeClass::Infra, Some("w2"), 250),
            reset_build_row(OutcomeClass::ResubmitReset, 1, 200),
            reset_build_row(OutcomeClass::PoisonCleared, 0, 350),
        ];
        for pos in 0..=mat_history.len() {
            for b in &build_rows {
                let mut h = mat_history.clone();
                h.insert(pos, b.clone());
                assert_eq!(
                    materialization_decide(&h, 2),
                    baseline,
                    "a build row at position {pos} changed the materialization decision"
                );
            }
        }
    }

    /// The kernel's dependency-free substring predicate agrees with
    /// `str::contains` for the CONCURRENT_PUTPATH marker over messages
    /// covering the interesting shapes: empty, shorter than the marker,
    /// the marker exact / prefixed / suffixed / embedded, near-misses
    /// sharing long prefixes with the marker, and partial occurrences
    /// ahead of a real one.
    #[test]
    fn concurrent_putpath_predicate_matches_std_contains() {
        let cases = [
            "",
            "x",
            "concurrent PutPath in progres",
            "concurrent PutPath in progress",
            "remote: concurrent PutPath in progress (path locked)",
            "concurrent putpath in progress",
            "concurrent PutPath in progressconcurrent PutPath in progress",
            "concurrent PutPath in progresX trailer",
            "cconcurrent PutPath in progress",
            "concurrent PutPath i_ progress but then concurrent PutPath in progress",
            "store error: connection reset by peer",
        ];
        for msg in cases {
            assert_eq!(
                contains_concurrent_putpath_marker(msg),
                msg.contains(CONCURRENT_PUTPATH_MSG),
                "predicate disagrees with str::contains for {msg:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Ledger GC sweep eligibility (sched.db.attempts-gc)
    // -----------------------------------------------------------------

    /// `sweep_eligible` truth table over the canonical hand history:
    /// only old attempt-kind rows strictly before the cut are eligible;
    /// post-reset age alone never deletes; reset rows are never
    /// eligible; a no-reset history has no cut and nothing eligible.
    // r[verify sched.db.attempts-gc]
    #[test]
    fn sweep_eligible_truth_table() {
        let budget = Budget::default();
        let horizon = sweep_horizon_secs(&budget, 86_400);
        let now: AbsTime = 200_000;

        // [attempt(old), attempt(old), reset, attempt(old), attempt(fresh)]
        let rows = vec![
            build_row(OutcomeClass::Transient, Some("w1"), 1),
            build_row(OutcomeClass::Infra, None, 2),
            reset_build_row(OutcomeClass::ResubmitReset, 1, 3),
            build_row(OutcomeClass::Transient, Some("w2"), 4),
            build_row(OutcomeClass::Transient, Some("w1"), 199_999),
        ];
        let eligible: Vec<bool> = (0..rows.len())
            .map(|i| sweep_eligible(&rows, i, now, horizon))
            .collect();
        assert_eq!(
            eligible,
            [true, true, false, false, false],
            "exactly the pre-cut old attempt rows; index 3 proves age \
             alone never deletes (post-reset), index 4 fails the age \
             conjunct"
        );

        // No reset row → no cut → nothing eligible, however old.
        let no_reset = vec![
            build_row(OutcomeClass::Transient, Some("w1"), 1),
            build_row(OutcomeClass::Infra, None, 2),
        ];
        assert!(
            (0..no_reset.len()).all(|i| !sweep_eligible(&no_reset, i, now, horizon)),
            "a history with no reset row is entirely live suffix"
        );

        // Reset rows are never eligible — even ancient pre-cut ones.
        let two_resets = vec![
            reset_build_row(OutcomeClass::CacheHitClear, 0, 1),
            reset_build_row(OutcomeClass::ResubmitReset, 1, 2),
        ];
        assert!(
            (0..two_resets.len()).all(|i| !sweep_eligible(&two_resets, i, now, horizon)),
            "reset rows of a live derivation are never deleted"
        );
    }

    /// `sweep_horizon_secs` is the max of the floor, the LIVE configured
    /// infra retry window, and the poison TTL — the attempts.rs
    /// "re-check against the configured value" clause.
    // r[verify sched.db.attempts-gc]
    #[test]
    fn sweep_horizon_dominates_floor_window_and_ttl() {
        // Production defaults: floor (== TTL, 86_400) dominates the
        // 300 s window.
        assert_eq!(sweep_horizon_secs(&Budget::default(), 86_400), 86_400);

        // Operator-widened infra window > floor → the window widens
        // retention with it.
        let wide = Budget {
            infra_retry_window_secs: 200_000,
            ..Budget::default()
        };
        assert_eq!(sweep_horizon_secs(&wide, 86_400), 200_000);

        // A poison TTL outgrowing the floor enters the max (today the
        // scheduler's compile guard keeps floor >= TTL; this pins the
        // term anyway).
        let ttl = Budget {
            poison_ttl_secs: 100_000,
            ..Budget::default()
        };
        assert_eq!(sweep_horizon_secs(&ttl, 86_400), 100_000);
    }

    /// sched.db.attempts-gc, exhaustively: over ALL histories of length
    /// <= 4 drawn from an 8-shape alphabet (three build attempt shapes
    /// × three build reset shapes — deliberately including the
    /// ResubmitReset cycle seed and the CacheHitClear backoff
    /// carve-out — plus a materialization-infra charge and a
    /// materialization-lane reset, so every cross-lane interleaving is
    /// inside the domain) × ALL subsets of fully-eligible indices: the
    /// LOADED VIEW (`row_survives_load`, the per-lane cut) is
    /// element-wise unchanged, decide()/materialization_decide() over
    /// it are bit-identical, and the per-lane loader cut itself
    /// preserves `materialization_decide` relative to the FULL history
    /// (the loader-cut theorem the kani harness
    /// `check_loader_cut_preserves_materialization_decide` proves over
    /// symbolic rows). The dependency-free bounded-exhaustive twin of
    /// the kani harnesses — runs under every cfg.
    // r[verify sched.db.attempts-gc]
    #[test]
    fn decide_invariant_under_eligible_sweep_exhaustive() {
        fn shape(code: usize, at: AbsTime) -> LedgerRow<String> {
            match code {
                0 => build_row(OutcomeClass::Transient, Some("w1"), at),
                1 => build_row(OutcomeClass::Infra, None, at),
                2 => build_row(OutcomeClass::ExecutorCrash, Some("w2"), at),
                3 => reset_build_row(OutcomeClass::ResubmitReset, 2, at),
                4 => reset_build_row(OutcomeClass::CacheHitClear, 0, at),
                5 => reset_build_row(OutcomeClass::PoisonCleared, 0, at),
                6 => mat_row(OutcomeClass::MaterializationInfra, Some("s1"), at),
                _ => LedgerRow {
                    event_kind: AttemptEventKind::Reset,
                    reporting_party: ReportingParty::Scheduler,
                    ..mat_row(OutcomeClass::ResubmitReset, None, at)
                },
            }
        }

        fn loaded_view(rows: &[LedgerRow<String>]) -> Vec<LedgerRow<String>> {
            (0..rows.len())
                .filter(|&i| row_survives_load(rows, i))
                .map(|i| rows[i].clone())
                .collect()
        }

        let budget = Budget::default();
        let horizon = sweep_horizon_secs(&budget, 86_400);
        let now: AbsTime = 1_000_000;
        let mut cases: u64 = 0;

        for len in 0..=4usize {
            for history_code in 0..8usize.pow(u32::try_from(len).expect("small")) {
                let mut code = history_code;
                let mut rows: Vec<LedgerRow<String>> = Vec::with_capacity(len);
                for position in 0..len {
                    // Ancient timestamps (0..=3): every row passes the
                    // age conjunct, so eligibility is decided by the
                    // structural predicate (the truth-table test owns
                    // the age corner).
                    rows.push(shape(code % 8, position as AbsTime));
                    code /= 8;
                }

                // The loader-cut theorem: the per-lane view loses no
                // materialization-decision information relative to the
                // full history (materialization_decide cuts at the mat
                // lane's own last reset, which the view preserves).
                assert_eq!(
                    materialization_decide(&loaded_view(&rows), 1),
                    materialization_decide(&rows, 1),
                    "per-lane loader cut must preserve the materialization \
                     verdict: history {rows:?}"
                );

                let eligible: Vec<usize> = (0..len)
                    .filter(|&i| sweep_eligible(&rows, i, now, horizon))
                    .collect();

                for selection in 0..(1usize << eligible.len()) {
                    let mut deleted = vec![false; len];
                    for (bit, &idx) in eligible.iter().enumerate() {
                        if selection & (1 << bit) != 0 {
                            deleted[idx] = true;
                        }
                    }
                    let swept: Vec<LedgerRow<String>> = rows
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !deleted[*i])
                        .map(|(_, r)| r.clone())
                        .collect();

                    let v1 = loaded_view(&rows);
                    let v2 = loaded_view(&swept);
                    assert_eq!(
                        v1, v2,
                        "loaded view changed under sweep: history {rows:?}, deleted {deleted:?}"
                    );
                    assert_eq!(decide(&v1, &budget, now), decide(&v2, &budget, now),);
                    assert_eq!(
                        materialization_decide(&v1, 1),
                        materialization_decide(&v2, 1),
                    );
                    cases += 1;
                }
            }
        }

        assert!(cases > 1_500, "case-count sanity: {cases}");
    }

    /// The kernel half of merged_bug_011, as a named pin: two
    /// materialization-infra charges followed by a BUILD resubmit reset
    /// — the per-lane loaded view keeps both charges, so
    /// `materialization_decide` still parks. Under the pre-084 any-kind
    /// cut the build reset truncated the whole history to itself and
    /// the loaded verdict came back `Claimable` (the parked-job
    /// resurrection this workstream closes); the DB twin
    /// (`test_build_reset_preserves_materialization_lane_in_loader`)
    /// recorded that red against the live SQL.
    // r[verify sched.db.attempts-gc]
    #[test]
    fn build_reset_does_not_cut_materialization_lane() {
        let rows = vec![
            mat_row(OutcomeClass::MaterializationInfra, Some("s1"), 1),
            mat_row(OutcomeClass::MaterializationInfra, Some("s1"), 2),
            reset_build_row(OutcomeClass::ResubmitReset, 1, 3),
        ];
        let view: Vec<LedgerRow<String>> = (0..rows.len())
            .filter(|&i| row_survives_load(&rows, i))
            .map(|i| rows[i].clone())
            .collect();
        assert_eq!(view, rows, "no mat reset → the mat lane has no cut");
        assert_eq!(
            materialization_decide(&view, 2),
            MaterializationVerdict::Park,
            "both charges visible: the budget stays exhausted across a build reset"
        );

        // And the mirror: a materialization-lane reset cuts ONLY the
        // mat lane — the build suffix is untouched.
        let mirror = vec![
            build_row(OutcomeClass::Infra, Some("w1"), 1),
            LedgerRow {
                event_kind: AttemptEventKind::Reset,
                reporting_party: ReportingParty::Scheduler,
                ..mat_row(OutcomeClass::ResubmitReset, None, 2)
            },
            mat_row(OutcomeClass::MaterializationInfra, Some("s1"), 3),
        ];
        assert!(
            (0..mirror.len()).all(|i| row_survives_load(&mirror, i)),
            "the build row survives a mat-lane reset; the mat charge is post-cut"
        );
    }
}

#[cfg(kani)]
mod proofs {
    //! CBMC proof harnesses for the decision kernels (`decide` /
    //! `classify` / `placeable` and the fold's counter arithmetic).
    //!
    //! Domain bounds, stated once: histories are bounded at 3–4 rows
    //! over a three-executor universe, budgets are scaled to 0..=2 (so
    //! every cap, threshold, and TTL terminal is reachable inside the
    //! bound — the same scaling `retryPolicy.qnt`'s regimes use), and
    //! the abstract clock is bounded at small values. The row domain
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
    //! The executor-identity type is `u8` here: the kernels are generic
    //! over `Id` and only ever use its `Ord`/`Eq`/`Clone`
    //! surface, so the verdict logic proven for `u8` identities is the
    //! same code production runs with `String` identities — and the
    //! solver never has to model heap allocation or string comparison.
    //! A row with no recorded executor identity folds as `None`
    //! (charges flat counters, contributes no exclusion key — decision
    //! P12); the named universe is {1, 2, 3}.
    //!
    //! Under `cfg(kani)` every executor-id set in the kernel and in
    //! these harnesses is [`BoundedIdSet`] (via the [`IdSet`] alias): a
    //! fixed-capacity array set whose operations are concretely bounded
    //! scans, so the goto model carries no b-tree node or heap
    //! machinery. `check_bounded_set_models_set_semantics` pins the
    //! representation itself to set semantics over symbolic values; the
    //! BTreeSet-differential unit tests in `mod tests` pin it to the
    //! production representation. Symbolic scalars are generated as
    //! widened bytes (`small_time`, the scaled-budget fields) so the
    //! clock and budget arithmetic goes through structurally narrow
    //! circuits. Unwind bounds: 7 covers the capacity-bounded set scans
    //! and the ≤4-row history folds (each at most four iterations plus
    //! margin) everywhere except the classification harness, whose 64
    //! covers the windowed byte comparison over the longest
    //! representative message.
    //!
    //! The tracey verify markers for these harnesses live at the
    //! `kani-rio-retry-kernel` wiring point in nix/kani.nix, not here —
    //! same discipline as the VM-test subtests list.

    use super::*;

    /// One arbitrary executor id from the named universe {1, 2, 3}:
    /// enough to reach the distinct-worker threshold at its scaled
    /// bound (2) with one spare so exclusion ⊂ fleet and exclusion ⊇
    /// fleet are both reachable in `check_placeable_contract` (a row
    /// with no identity is generated as `executor: None`).
    fn any_executor() -> u8 {
        let i: u8 = kani::any();
        kani::assume(i >= 1 && i <= 3);
        i
    }

    /// A symbolic instant on the abstract clock, generated as a widened
    /// byte so the high 56 bits are structurally zero: the clock
    /// arithmetic (window reset, TTL, backoff deadlines) then goes
    /// through narrow circuits instead of full 64-bit ones. The bounded
    /// domains never need more than a byte of clock anyway.
    fn small_time(bound: u8) -> AbsTime {
        let v: u8 = kani::any();
        kani::assume(v <= bound);
        AbsTime::from(v)
    }

    /// Every outcome class, including the two materialization classes
    /// and the store-degraded pacing class (the full 16-literal
    /// alphabet): the row domain stays a strict
    /// superset of what any appending site can write, so kind/class
    /// combinations that are malformed by writer discipline (e.g. a
    /// build-kind row carrying a materialization class) are inside the
    /// proven domain and covered by the fold's no-op arms.
    fn any_outcome_class() -> OutcomeClass {
        let i: u8 = kani::any();
        kani::assume(i < 17);
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
            12 => OutcomeClass::PoisonCleared,
            13 => OutcomeClass::MaterializationUnobtainable,
            14 => OutcomeClass::MaterializationInfra,
            15 => OutcomeClass::MaterializationReset,
            _ => OutcomeClass::StoreDegraded,
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

    /// One arbitrary ledger row: every decision-relevant field free
    /// (class, work kind, flags, party, executor, cycle, timestamp). The
    /// fields `decide()` ignores (UUIDs, error messages, the recorded-at
    /// timestamp) are not part of [`LedgerRow`] at all — the scheduler's
    /// projection shim drops them before the kernel ever sees a row.
    fn any_row(max_at: u8) -> LedgerRow<u8> {
        let cycle: u8 = kani::any();
        kani::assume(cycle <= 3);
        LedgerRow {
            event_kind: if kani::any() {
                AttemptEventKind::Attempt
            } else {
                AttemptEventKind::Reset
            },
            outcome_class: any_outcome_class(),
            executor: if kani::any() {
                Some(any_executor())
            } else {
                None
            },
            reporting_party: any_reporting_party(),
            floor_promoted: kani::any(),
            floor_at_cap: kani::any(),
            resubmit_cycle: i32::from(cycle),
            at: small_time(max_at),
            // Symbolic work kind: the kind-partition harnesses need
            // both kinds in the domain, and every other harness must
            // hold over rows of either kind.
            kind: if kani::any() {
                AttemptKind::Build
            } else {
                AttemptKind::Materialization
            },
        }
    }

    /// A bounded arbitrary suffix: `MAX` arbitrary rows plus a symbolic
    /// length `n <= MAX`; harnesses fold `&rows[..n]`. A fixed array +
    /// symbolic-length slice keeps the suffix construction free of heap
    /// allocation and growth loops (the same shape rio-store's
    /// manifest-coverage harness uses), which keeps the goto model
    /// smaller than a `Vec` push loop would.
    fn any_history<const MAX: usize>() -> ([LedgerRow<u8>; MAX], usize) {
        let rows = [(); MAX].map(|_| any_row(8));
        let n: usize = kani::any();
        kani::assume(n <= MAX);
        (rows, n)
    }

    /// Budgets scaled so every terminal is reachable within the history
    /// bound. The contract must hold for every configuration, so zero
    /// caps and both threshold modes are included. The fields are
    /// generated as widened bytes (same rationale as [`small_time`]) so
    /// the cap comparisons and the backoff multiplication stay narrow.
    fn any_small_budget() -> Budget {
        let small_u32 = |bound: u8| -> u32 {
            let v: u8 = kani::any();
            kani::assume(v <= bound);
            u32::from(v)
        };
        let small_u64 = |bound: u8| -> u64 {
            let v: u8 = kani::any();
            kani::assume(v <= bound);
            u64::from(v)
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

    /// The proof-time set representation itself models set semantics
    /// over symbolic values: insert reports newness exactly when the
    /// value was absent, membership tracks exactly the inserted values,
    /// `len` counts distinct values, insertion order does not affect
    /// membership or size, and `iter()` yields exactly the members.
    /// This is the harness-side half of the equivalence pin for the
    /// cfg(kani) [`IdSet`] swap (the BTreeSet-differential half runs as
    /// a unit test under every cfg).
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_bounded_set_models_set_semantics() {
        let a: u8 = kani::any();
        let b: u8 = kani::any();
        let probe: u8 = kani::any();

        let mut s = BoundedIdSet::<u8>::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(!s.contains(&probe));

        // First insert: newness reported, membership and size update.
        assert!(s.insert(a));
        assert!(s.contains(&a));
        assert!(!s.is_empty());
        assert_eq!(s.len(), 1);
        // Re-insert is idempotent and reports the value as seen.
        assert!(!s.insert(a));
        assert_eq!(s.len(), 1);

        // Second value: newness iff distinct, size reflects it.
        let b_was_new = s.insert(b);
        assert_eq!(b_was_new, b != a);
        assert!(s.contains(&b));
        assert_eq!(s.len(), if b == a { 1 } else { 2 });

        // Membership is precise: never-inserted values stay out.
        if probe != a && probe != b {
            assert!(!s.contains(&probe));
        }

        // Insertion order does not change membership or size.
        let mut t = BoundedIdSet::<u8>::new();
        t.insert(b);
        t.insert(a);
        assert_eq!(t.len(), s.len());
        assert!(t.contains(&a));
        assert!(t.contains(&b));

        // iter() yields exactly the members: every yielded value is a
        // member and the yield count matches len().
        let mut yielded = 0;
        for v in s.iter() {
            assert!(*v == a || *v == b);
            yielded += 1;
        }
        assert_eq!(yielded, s.len());
    }

    /// Verify [`decide`] against its three stated `kani::ensures`
    /// clauses — the verdict partition, the Requeue cap bounds, and the
    /// exclusion-set superset — for every
    /// suffix of up to 4 arbitrary rows, every scaled budget, and every
    /// clock value up to 16. With
    /// overflow checks on, the same run is the no-overflow proof for
    /// the fold's counter arithmetic over that domain.
    ///
    /// The clauses are asserted via the shared `decide_*` predicate
    /// functions (the same bodies the `#[kani::ensures]` attributes
    /// wrap) on the result of a plain call, NOT via
    /// `#[kani::proof_for_contract(decide)]`: the contract-instrumented
    /// wrapper around the whole fold does not converge inside the
    /// merge-gate budget, while the assert form proves the identical
    /// clauses over the identical domain.
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_decide_contract() {
        let (rows, n) = any_history::<4>();
        let history = &rows[..n];
        let budget = any_small_budget();
        let now = small_time(16);
        let d = decide(history, &budget, now);
        assert!(decide_verdict_partition_consistent(&d, &budget, now));
        assert!(decide_requeue_within_caps(&d, history, &budget));
        assert!(decide_exclusion_covers_charged_attempts(&d, history));
    }

    /// The verdict partition is deterministic: two calls on the same
    /// (history, budget, now) triple return the same Decision
    /// — no hidden state, no clock other than `now`, no dependence on
    /// set iteration order.
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_decide_deterministic() {
        let (rows, n) = any_history::<2>();
        let history = &rows[..n];
        let budget = any_small_budget();
        let now = small_time(16);
        let a = decide(history, &budget, now);
        let b = decide(history, &budget, now);
        assert_eq!(a, b);
    }

    /// Verify [`classify`] against its partition contract for every
    /// observed-failure variant, every floor outcome, and four
    /// representative error messages (empty, unrelated, the
    /// CONCURRENT_PUTPATH marker verbatim, the marker embedded
    /// mid-string) — the shapes the substring predicate distinguishes.
    ///
    /// The unwind bound covers the substring search over the longest of
    /// the four messages (52 bytes) plus the 28-byte marker; without an
    /// explicit bound CBMC keeps unwinding the search/compare loops far
    /// past anything the concrete messages can reach.
    // r[verify sched.retry.store-degraded-uncharged+2]
    /// bug_408: a history whose ATTEMPT rows are all build-lane
    /// store-degraded (reset rows and materialization-kind rows may
    /// interleave freely) charges nothing — every count counter zero,
    /// exclusion empty, never a poison verdict — and the charging
    /// alphabet cannot even represent the class (`row_to_event`
    /// answers `None`). Bounded at MAX=4 / unwind 7 per the crate's
    /// harness conventions.
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_store_degraded_uncharged_requeue() {
        const MAX: usize = 4;
        let (mut rows, n) = any_history::<MAX>();
        for row in &mut rows {
            if row.event_kind == AttemptEventKind::Attempt && row.kind == AttemptKind::Build {
                row.outcome_class = OutcomeClass::StoreDegraded;
            }
        }
        for row in &rows[..n] {
            if row.event_kind == AttemptEventKind::Attempt
                && row.outcome_class == OutcomeClass::StoreDegraded
            {
                kani::assert(
                    row_to_event(row).is_none(),
                    "store-degraded is unrepresentable in the charging alphabet",
                );
            }
        }
        let budget = any_small_budget();
        let now = small_time(8);
        let d = decide(&rows[..n], &budget, now);
        kani::assert(
            !matches!(d.verdict, Verdict::Poison(_)),
            "store-degraded rows never poison",
        );
        kani::assert(d.exclusion.is_empty(), "no exclusion key is ever minted");
        kani::assert(
            d.counters.count == 0
                && d.counters.infra_count == 0
                && d.counters.timeout_count == 0
                && d.counters.exempt_infra_count == 0
                && d.counters.failure_count == 0
                && d.counters.poisoned_at.is_none(),
            "no count budget advances",
        );
    }

    #[kani::proof_for_contract(classify)]
    #[kani::unwind(64)]
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
            2 => CONCURRENT_PUTPATH_MSG,
            _ => "remote: concurrent PutPath in progress (path locked)",
        };
        let ev_sel: u8 = kani::any();
        kani::assume(ev_sel < 10);
        let event = match ev_sel {
            0 => ObservedFailure::WorkerTransient,
            1 => ObservedFailure::WorkerInfra { error_msg },
            2 => ObservedFailure::WorkerPermanent,
            3 => ObservedFailure::WorkerTimeout,
            4 => ObservedFailure::Disconnect,
            5 => ObservedFailure::ControllerResourceTermination,
            6 => ObservedFailure::ControllerDeadlineExceeded,
            7 => ObservedFailure::BackstopTimeout,
            8 => ObservedFailure::UnreportedCrash,
            _ => ObservedFailure::WorkerStoreDegraded,
        };
        let _ = classify(&event, floor);
    }

    /// One arbitrary subset of the executor universe.
    fn any_id_set() -> IdSet<u8> {
        let mut s = IdSet::new();
        if kani::any() {
            s.insert(1u8);
        }
        if kani::any() {
            s.insert(2u8);
        }
        if kani::any() {
            s.insert(3u8);
        }
        s
    }

    /// Verify [`placeable`] against its partition contract for every
    /// (exclusion, fleet) pair over the three-executor universe —
    /// including the empty fleet (defer, never poison), the empty
    /// exclusion set (always placeable), full overlap (exhausted), and
    /// every partial overlap.
    #[kani::proof_for_contract(placeable)]
    #[kani::unwind(7)]
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
    /// empty-fleet defer clause of `sched.dispatch.fleet-exhaust+5`).
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_fold_fleet_exhaust_arm() {
        // Three arbitrary worker-reported events plus a symbolic length,
        // array-backed for the same reason as `any_history`.
        let events = [(); 3].map(|_| {
            let at = small_time(8);
            if kani::any() {
                AttemptEvent::Transient {
                    at,
                    executor: Some(any_executor()),
                }
            } else {
                AttemptEvent::Infra {
                    at,
                    executor: Some(any_executor()),
                    exempt: kani::any(),
                    at_cap: kani::any(),
                }
            }
        });
        let n: usize = kani::any();
        kani::assume(n <= 3);
        let history = &events[..n];
        let mut fleet = FleetView::default();
        if kani::any() {
            fleet.eligible.insert(1u8);
        }
        if kani::any() {
            fleet.eligible.insert(2u8);
        }
        let now = small_time(16);
        let (counters, verdict) = reference_fold(history, now, &Budget::default(), &fleet);
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

    /// The build-kind-only view of a bounded history: clone-initialised
    /// fixed array + compacted prefix length (no heap, no Vec growth —
    /// the same array discipline as [`any_history`]). Shared by the two
    /// kind-partition harnesses below.
    fn build_only_view<const MAX: usize>(
        rows: &[LedgerRow<u8>; MAX],
        n: usize,
    ) -> ([LedgerRow<u8>; MAX], usize) {
        let mut filtered: [LedgerRow<u8>; MAX] = rows.clone();
        let mut m = 0;
        for row in rows.iter().take(n) {
            if row.kind == AttemptKind::Build {
                filtered[m] = row.clone();
                m += 1;
            }
        }
        (filtered, m)
    }

    /// materializationInvisibleToBuildBudgets, kernel half
    /// (substitution-replacement design §2.5, review findings PP-4/BC-2):
    /// for ANY row set over the full symbolic domain — both work kinds,
    /// all 15 outcome classes, all parties/flags/identities — the build
    /// decision over the full set equals the build decision over its
    /// build-kind-only subset. Verdict, exclusion set, backoff deadline,
    /// and every counter: materialization rows are invisible to all of
    /// them, wherever they sit in the history (including ahead of the
    /// resubmit-cycle seed row).
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_materialization_rows_invisible_to_build_decision() {
        let (rows, n) = any_history::<4>();
        let history = &rows[..n];
        let budget = any_small_budget();
        let now = small_time(16);

        let full = decide(history, &budget, now);

        let (filtered, m) = build_only_view(&rows, n);
        let build_only = decide(&filtered[..m], &budget, now);

        assert_eq!(full, build_only);
    }

    /// materializationNeverPoisons, kernel half: no row set produces a
    /// Poison verdict attributable to its materialization-kind rows —
    /// if the build-kind subset alone does not poison, the full set
    /// (with any number of materialization rows, any classes) does not
    /// poison either. A corollary of the invisibility property, stated
    /// separately because it is the clause the park-not-fail posture
    /// (sched.materialize.routing, B3 "unknown never demotes") rests on.
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_materialization_never_poisons() {
        let (rows, n) = any_history::<4>();
        let history = &rows[..n];
        let budget = any_small_budget();
        let now = small_time(16);

        let full = decide(history, &budget, now);

        let (filtered, m) = build_only_view(&rows, n);
        let build_only = decide(&filtered[..m], &budget, now);

        if !matches!(build_only.verdict, Verdict::Poison(_)) {
            assert!(!matches!(full.verdict, Verdict::Poison(_)));
        }
    }

    /// Compact the non-deleted rows of `rows[..n]` under `mask` into a
    /// fresh fixed array (index loop, no Vec — the CBMC-blowup lesson):
    /// the harness-side model of the sweep's DELETE.
    fn sweep_view<const MAX: usize>(
        rows: &[LedgerRow<u8>; MAX],
        n: usize,
        mask: &[bool; MAX],
    ) -> ([LedgerRow<u8>; MAX], usize) {
        let mut swept = rows.clone();
        let mut m = 0;
        let mut i = 0;
        while i < n {
            if !mask[i] {
                swept[m] = rows[i].clone();
                m += 1;
            }
            i += 1;
        }
        (swept, m)
    }

    /// The loaded view of `rows[..n]` under the per-lane cut
    /// (`row_survives_load`), compacted into a fresh fixed array (index
    /// loop, no Vec — the CBMC-blowup lesson): the harness-side model
    /// of what the per-lane loaders return.
    fn loaded_view<const MAX: usize>(
        rows: &[LedgerRow<u8>; MAX],
        n: usize,
    ) -> ([LedgerRow<u8>; MAX], usize) {
        let mut view = rows.clone();
        let mut m = 0;
        let mut i = 0;
        while i < n {
            if row_survives_load(&rows[..n], i) {
                view[m] = rows[i].clone();
                m += 1;
            }
            i += 1;
        }
        (view, m)
    }

    /// sched.db.attempts-gc, structural half (re-stated over the
    /// per-lane cut, migration 084): for EVERY bounded history and
    /// EVERY deletion mask confined to attempt-kind rows strictly
    /// before THEIR OWN lane's last reset row (`sweep_eligible`'s
    /// structural conjuncts E1+E2, kinded; deliberately WITHOUT the age
    /// conjunct, so every age implementation — any clock, any skew — is
    /// covered as a mask-shrinking special case), the LOADED VIEW
    /// (`row_survives_load`) is element-wise unchanged. No decide()
    /// call — cheap row comparisons only, which is what lets this
    /// harness carry the larger MAX=5 bound.
    #[kani::proof]
    #[kani::unwind(8)]
    fn check_sweep_suffix_equivalence() {
        const MAX: usize = 5;
        let (rows, n) = any_history::<MAX>();
        let mask: [bool; MAX] = [(); MAX].map(|_| kani::any());
        let mut i = 0;
        while i < MAX {
            kani::assume(
                !mask[i]
                    || (i < n
                        && rows[i].event_kind == AttemptEventKind::Attempt
                        && i < ledger_suffix_start(&rows[..n], rows[i].kind)),
            );
            i += 1;
        }

        let (swept, m) = sweep_view(&rows, n, &mask);

        let (v1, l1) = loaded_view(&rows, n);
        let (v2, l2) = loaded_view(&swept, m);
        assert_eq!(l1, l2);
        let mut k = 0;
        while k < l1 {
            assert_eq!(v1[k], v2[k]);
            k += 1;
        }
    }

    /// sched.db.attempts-gc, end-to-end half (re-stated over the
    /// per-lane cut): the loader-composed theorem itself — `decide()`
    /// and `materialization_decide()` over the LOADED VIEW are
    /// bit-identical before and after any structural sweep. Folds
    /// decide() twice at MAX=4 (~2× check_decide_contract); documented
    /// fallback if it ever exceeds the gate budget: shrink to MAX=3 and
    /// record the measurement (the exhaustive unit test keeps the
    /// len<=4 equivalence machine-checked under every cfg regardless).
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_sweep_decide_invariant() {
        const MAX: usize = 4;
        let (rows, n) = any_history::<MAX>();
        let mask: [bool; MAX] = [(); MAX].map(|_| kani::any());
        let mut i = 0;
        while i < MAX {
            kani::assume(
                !mask[i]
                    || (i < n
                        && rows[i].event_kind == AttemptEventKind::Attempt
                        && i < ledger_suffix_start(&rows[..n], rows[i].kind)),
            );
            i += 1;
        }

        let (swept, m) = sweep_view(&rows, n, &mask);

        let (v1, l1) = loaded_view(&rows, n);
        let (v2, l2) = loaded_view(&swept, m);

        let budget = any_small_budget();
        let now = small_time(16);
        let before = decide(&v1[..l1], &budget, now);
        let after = decide(&v2[..l2], &budget, now);
        assert_eq!(before, after);

        let k: u8 = kani::any();
        kani::assume(k <= 2);
        assert_eq!(
            materialization_decide(&v1[..l1], u32::from(k)),
            materialization_decide(&v2[..l2], u32::from(k)),
        );

        // A3: the sweep is invisible to the FULL counters fold, not
        // just the park projection — `materialization_counters` (the
        // [A] charge chokepoint's decision input: infra, worker-infra,
        // and unobtainable counts since the lane's reset cut) is
        // bit-identical over the swept view. `materialization_decide`
        // is its `infra_since_reset >= max` projection, so this
        // strictly subsumes the assertion above; both stay (the
        // projection identity is the wrapper contract the scheduler
        // relies on).
        assert_eq!(
            materialization_counters(&v1[..l1]),
            materialization_counters(&v2[..l2]),
        );
    }

    /// merged_bug_011, the loader-cut theorem: the per-lane loaded view
    /// loses NO materialization-decision information relative to the
    /// FULL history — `materialization_decide` over the view equals it
    /// over everything, for every bounded history and budget. (The
    /// fold's own window is the mat lane's last reset, which
    /// `row_survives_load` preserves by construction; under the pre-084
    /// any-kind cut a trailing build reset emptied the view and flipped
    /// parked verdicts back to Claimable.)
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_loader_cut_preserves_materialization_decide() {
        const MAX: usize = 4;
        let (rows, n) = any_history::<MAX>();
        let (view, m) = loaded_view(&rows, n);

        let k: u8 = kani::any();
        kani::assume(k <= 2);
        assert_eq!(
            materialization_decide(&view[..m], u32::from(k)),
            materialization_decide(&rows[..n], u32::from(k)),
        );
    }

    /// sched.attempt.worker-abort-bounded (bug_279): for EVERY bounded
    /// history, `admit_worker_abort` answers `Uncharged` iff the
    /// trailing build-lane free-close run is strictly below the bound
    /// (the spec-side recount below is a forward scan — a structurally
    /// different fold from the production reverse-break loop); and the
    /// uncharged close the admission authorizes (appending exactly the
    /// free-close row pull-mode writes) STRICTLY grows that run — so
    /// consecutive uncharged admissions are bounded at
    /// `WORKER_ABORT_FREE_CLOSES` by induction.
    // r[verify sched.attempt.worker-abort-bounded]
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_worker_abort_bounded() {
        const MAX: usize = 4;

        /// Forward-scan recount: reset on any non-free build-lane row,
        /// skip materialization-lane rows.
        fn trailing_free_run(rows: &[LedgerRow<u8>]) -> u32 {
            let mut run: u32 = 0;
            let mut i = 0;
            while i < rows.len() {
                let r = &rows[i];
                if r.kind == AttemptKind::Build {
                    let is_free = r.event_kind == AttemptEventKind::Attempt
                        && r.outcome_class == OutcomeClass::Disconnected
                        && r.reporting_party == ReportingParty::Worker;
                    run = if is_free { run + 1 } else { 0 };
                }
                i += 1;
            }
            run
        }

        let (mut rows, n) = any_history::<MAX>();
        kani::assume(n < MAX); // room for the appended close

        let admit = admit_worker_abort(&rows[..n], WORKER_ABORT_FREE_CLOSES);
        let run = trailing_free_run(&rows[..n]);
        assert!((admit == WorkerAbortAdmission::Uncharged) == (run < WORKER_ABORT_FREE_CLOSES));

        // Growth lemma: the authorized close appends a free-close row
        // (arbitrary time/cycle/floor metadata — only the four
        // discriminant fields are pinned, exactly what the uncharged
        // close writes).
        rows[n].kind = AttemptKind::Build;
        rows[n].event_kind = AttemptEventKind::Attempt;
        rows[n].outcome_class = OutcomeClass::Disconnected;
        rows[n].reporting_party = ReportingParty::Worker;
        assert!(trailing_free_run(&rows[..n + 1]) == run + 1);
    }

    /// sched.retry.store-degraded-uncharged (merged_bug_032): for EVERY
    /// bounded history, `admit_store_degraded` answers `Uncharged` iff
    /// the trailing build-lane store-degraded run is strictly below the
    /// bound (forward-scan recount vs the production reverse-break
    /// loop); and the paced write the admission authorizes STRICTLY
    /// grows that run — so consecutive uncharged store-degraded
    /// admissions are bounded at `STORE_DEGRADED_FREE_RUN` by
    /// induction, closing the worker-supplied unbounded-mint.
    // r[verify sched.retry.store-degraded-uncharged+2]
    #[kani::proof]
    #[kani::unwind(7)]
    fn check_store_degraded_admission_bounded() {
        const MAX: usize = 4;
        // A small symbolic bound (≤3) keeps the proof domain-complete
        // without unwinding the production constant's 12-row run; the
        // induction transfers to any bound.
        let bound: u32 = kani::any();
        kani::assume(bound >= 1 && bound <= 3);

        /// Forward-scan recount: reset on any non-paced build-lane
        /// row, skip materialization-lane rows.
        fn trailing_paced_run(rows: &[LedgerRow<u8>]) -> u32 {
            let mut run: u32 = 0;
            let mut i = 0;
            while i < rows.len() {
                let r = &rows[i];
                if r.kind == AttemptKind::Build {
                    let is_paced = r.event_kind == AttemptEventKind::Attempt
                        && r.outcome_class == OutcomeClass::StoreDegraded
                        && r.reporting_party == ReportingParty::Worker;
                    run = if is_paced { run + 1 } else { 0 };
                }
                i += 1;
            }
            run
        }

        let (mut rows, n) = any_history::<MAX>();
        kani::assume(n < MAX); // room for the appended paced row

        let admit = admit_store_degraded(&rows[..n], bound);
        let run = trailing_paced_run(&rows[..n]);
        assert!((admit == WorkerAbortAdmission::Uncharged) == (run < bound));

        // Growth lemma: the authorized paced write appends exactly the
        // four pinned discriminants.
        rows[n].kind = AttemptKind::Build;
        rows[n].event_kind = AttemptEventKind::Attempt;
        rows[n].outcome_class = OutcomeClass::StoreDegraded;
        rows[n].reporting_party = ReportingParty::Worker;
        assert!(trailing_paced_run(&rows[..n + 1]) == run + 1);
    }

    // r[verify store.log.sweep-ownership]
    /// [`exec_row_sweep_eligible`] is exactly the four-conjunct guard:
    /// eligibility implies every safety conjunct (terminal, no active
    /// assignment, no ledger reference, aged out), and any single
    /// violated conjunct vetoes — the second deleter of execution rows
    /// can never weaken to a disjunction or drop a guard without this
    /// harness failing.
    #[kani::proof]
    fn check_exec_row_sweep_guards() {
        let terminal: bool = kani::any();
        let active: bool = kani::any();
        let referenced: bool = kani::any();
        let aged: bool = kani::any();

        let eligible = exec_row_sweep_eligible(terminal, active, referenced, aged);
        if eligible {
            assert!(terminal);
            assert!(!active);
            assert!(!referenced);
            assert!(aged);
        }
        if !terminal || active || referenced || !aged {
            assert!(!eligible);
        }
    }

    /// `materialization_counters` window theorem (the [A] charge
    /// chokepoint's decision input): the three counts depend ONLY on
    /// the materialization lane's reset-cut suffix — recomputing over
    /// `rows[ledger_suffix_start(rows, Materialization)..]` is
    /// bit-identical, the worker subset is bounded by the total, and
    /// `materialization_decide` is exactly the `infra_since_reset >=
    /// max` projection of the same fold (the wrapper identity the
    /// scheduler's chokepoint relies on). MAX=5/unwind 8, the sweep
    /// theorems' bound family.
    #[kani::proof]
    #[kani::unwind(8)]
    fn check_materialization_counters_window() {
        const MAX: usize = 5;
        let (rows, n) = any_history::<MAX>();
        let rows = &rows[..n];

        let c = materialization_counters(rows);
        let cut = ledger_suffix_start(rows, AttemptKind::Materialization);
        let c_suffix = materialization_counters(&rows[cut..]);
        assert!(c == c_suffix);
        assert!(c.worker_infra_since_reset <= c.infra_since_reset);

        let max: u8 = kani::any();
        let max = u32::from(max);
        let parked = materialization_decide(rows, max) == MaterializationVerdict::Park;
        assert!(parked == (c.infra_since_reset >= max));
    }
}
