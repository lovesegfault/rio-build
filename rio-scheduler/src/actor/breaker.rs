//! Circuit breaker for cache-check failures.
//!
//! Without this, a sustained store outage means every SubmitBuild treats
//! every derivation as a cache miss — an avalanche of unnecessary rebuilds
//! once the store comes back (or workers thrash trying to fetch inputs).
//!
//! Pattern reference: `rio-builder/src/fuse/circuit.rs` has the same
//! 3-state shape but uses `AtomicU32` (fuser's thread pool is
//! multi-threaded). This one is actor-local — single-threaded `&mut self`
//! access — so plain `u32` suffices.
//!
//! WONTFIX: scheduler and builder breakers intentionally diverge on
//! interior mutability (plain u32 actor-local vs AtomicU32). Consolidate
//! only if a third breaker appears.

use std::time::Instant;

use tracing::{info, warn};

// ── Clock trait: testable auto-close without real sleeps ──────────────
// `tokio::time::pause` does NOT work here — this is `std::time::Instant`,
// not tokio's monotonic clock. Inject via trait so tests can advance a
// MockClock instead of sleeping 30s. Same shape as
// `rio-builder/src/fuse/circuit.rs`'s `Clock`.

/// Time source for the breaker. Production uses [`SystemClock`]; tests
/// inject a mock so the auto-close transition tests don't need real 30s
/// sleeps.
pub trait Clock {
    fn now(&self) -> Instant;
}

/// Production clock: `Instant::now()`.
#[derive(Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Circuit breaker for the scheduler's cache-check FindMissingPaths call.
///
/// Without this, a sustained store outage means every SubmitBuild treats
/// every derivation as a cache miss — an avalanche of unnecessary rebuilds
/// once the store comes back (or workers thrash trying to fetch inputs).
///
/// # States
///
/// - **Closed** (normal): cache check runs, result used. Tracks consecutive
///   failures; trips open after [`OPEN_THRESHOLD`].
/// - **Open**: cache check STILL runs (half-open probe), but if it fails,
///   SubmitBuild is rejected with `StoreUnavailable` instead of queueing.
///   If the probe succeeds, the breaker closes and the result is used.
///   Auto-closes after [`OPEN_DURATION`] even without a successful probe
///   (so a transient store blip doesn't lock us out forever if no builds
///   arrive to probe with).
///
/// # Why half-open probe, not skip-the-call
///
/// If we skipped the cache check entirely while open, the breaker could
/// only close via timeout. A successful probe is a faster, more responsive
/// signal that the store is back. The probe costs one RPC per SubmitBuild
/// — same as the closed state — so there's no extra load.
// r[impl sched.breaker.cache-check+2]
#[derive(Debug)]
pub(crate) struct CacheCheckBreaker<C: Clock = SystemClock> {
    /// Consecutive cache-check failures. Reset to 0 on any success.
    consecutive_failures: u32,
    /// If `Some`, the breaker is open until this instant. `None` = closed.
    open_until: Option<Instant>,
    /// Time source. `SystemClock` in production; `MockClock` in tests.
    clock: C,
}

// `Default` only for the production `SystemClock` parameterization so
// `CacheCheckBreaker::default()` (actor field init + existing tests)
// stays unambiguous. MockClock-parameterized tests use [`new`].
impl Default for CacheCheckBreaker {
    fn default() -> Self {
        Self::new()
    }
}

/// Trip open after this many consecutive failures.
const OPEN_THRESHOLD: u32 = 5;

/// Stay open for this long before auto-closing (even without a probe
/// success). Derived as `MERGE_FMP_TIMEOUT + 10 s` so a half-open
/// probe (which is the same FMP call) cannot outlive the open window —
/// otherwise the breaker auto-closes mid-probe, the probe's eventual
/// timeout sees `is_open()==false` → `lazy_reset_if_auto_closed`
/// resets the failure counter → only 1-in-`OPEN_THRESHOLD` submits is
/// rejected during a sustained outage. `find_missing_with_breaker`
/// further uses `grpc_timeout` (30 s) for the half-open probe so this
/// coupling is belt-and-suspenders.
const OPEN_DURATION: std::time::Duration =
    std::time::Duration::from_secs(super::MERGE_FMP_TIMEOUT.as_secs() + 10);

impl<C: Clock + Default> CacheCheckBreaker<C> {
    pub(super) fn new() -> Self {
        Self {
            consecutive_failures: 0,
            open_until: None,
            clock: C::default(),
        }
    }
}

impl<C: Clock> CacheCheckBreaker<C> {
    /// Lazy-reset on auto-close: if `open_until` has elapsed, return to
    /// true Closed semantics (5 fresh failures to re-trip; no duplicate
    /// open-transition metric). r[sched.breaker.cache-check+2] defines
    /// auto-close as returning to **Closed** — without this, the first
    /// post-timeout failure sees `consecutive_failures` still ≥5 and
    /// re-trips immediately, double-counting the same outage.
    fn lazy_reset_if_auto_closed(&mut self) {
        if self.open_until.is_some() && !self.is_open() {
            self.open_until = None;
            self.consecutive_failures = 0;
        }
    }

    /// Record a failure. Returns `true` if this failure trips the breaker open
    /// (or it was already open) — caller should reject SubmitBuild.
    ///
    /// The "already open" case matters: while open, we STILL attempt the cache
    /// check (half-open probe). A failed probe keeps us open but doesn't
    /// re-increment the open-transition metric (it's the same outage).
    pub(super) fn record_failure(&mut self) -> bool {
        self.lazy_reset_if_auto_closed();
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        // Already open: stay open. No metric (same outage, not a new trip).
        if self.is_open() {
            return true;
        }

        // Trip open on threshold crossing.
        if self.consecutive_failures >= OPEN_THRESHOLD {
            self.open_until = Some(self.clock.now() + OPEN_DURATION);
            warn!(
                consecutive_failures = self.consecutive_failures,
                open_for = ?OPEN_DURATION,
                "cache-check circuit breaker OPENING — rejecting SubmitBuild until store recovers"
            );
            metrics::counter!("rio_scheduler_cache_check_circuit_open_total").increment(1);
            return true;
        }

        // Still under threshold: proceed as if the cache check just missed.
        false
    }

    /// Record a success. Closes the breaker and resets the failure counter.
    pub(super) fn record_success(&mut self) {
        if self.is_open() {
            info!(
                after_failures = self.consecutive_failures,
                "cache-check circuit breaker CLOSING — store recovered"
            );
        }
        self.consecutive_failures = 0;
        self.open_until = None;
    }

    /// Whether the breaker is open RIGHT NOW. Handles timeout-based
    /// auto-close: if `open_until` is in the past, we're closed.
    ///
    /// Doesn't mutate state — the stale `open_until` is cleaned up lazily
    /// on the next `record_*()`. This keeps the check cheap (one
    /// `clock.now()` compare) and avoids needing `&mut self` here.
    ///
    /// `pub(super)`: merge.rs feeds the breaker via
    /// `find_missing_with_breaker` (record_* calls); completion.rs gates
    /// `verify_cutoff_candidates` on it (read-only — completion trusts
    /// the merge-time probe rather than re-feeding); debug.rs exposes it
    /// for tests. dispatch.rs deliberately does NOT gate (dispatch-time
    /// probe failure = cache-miss, not StoreUnavailable; the call IS the
    /// work, not an admission-time optimization).
    pub(super) fn is_open(&self) -> bool {
        self.open_until
            .is_some_and(|until| self.clock.now() < until)
    }
}

/// Breaker unit tests: the state machine in isolation, no actor needed.
/// These are cheap synchronous tests covering threshold/reset edges and
/// the auto-close transition (via `MockClock`).
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    /// Mockable Instant source. `Cell` so `&self` advance in tests.
    #[derive(Debug)]
    struct MockClock(Cell<Instant>);
    impl Default for MockClock {
        fn default() -> Self {
            Self(Cell::new(Instant::now()))
        }
    }
    impl MockClock {
        fn advance(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }
    impl Clock for MockClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    #[test]
    fn closed_under_threshold() {
        let mut b = CacheCheckBreaker::default();
        for i in 1..=4 {
            assert!(!b.record_failure(), "failure {i} should not trip");
        }
    }

    #[test]
    fn trips_at_threshold() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=4 {
            b.record_failure();
        }
        assert!(b.record_failure(), "5th failure should trip");
    }

    #[test]
    fn stays_open_on_subsequent_failures() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=5 {
            b.record_failure();
        }
        // 6th, 7th, ... — still returns true (already open).
        assert!(b.record_failure());
        assert!(b.record_failure());
    }

    #[test]
    fn success_closes_and_resets() {
        let mut b = CacheCheckBreaker::default();
        for _ in 1..=5 {
            b.record_failure();
        }
        b.record_success();
        // Back to needing 5 more failures.
        for i in 1..=4 {
            assert!(
                !b.record_failure(),
                "post-reset failure {i} should not trip"
            );
        }
        assert!(b.record_failure(), "post-reset 5th should trip again");
    }

    #[test]
    fn success_while_closed_is_cheap_noop() {
        let mut b = CacheCheckBreaker::default();
        b.record_failure();
        b.record_failure();
        b.record_success(); // resets counter to 0
        // Need 5 MORE failures now, not 3.
        for i in 1..=4 {
            assert!(!b.record_failure(), "failure {i} should not trip");
        }
    }

    #[test]
    fn saturating_add_no_overflow() {
        // u32::MAX failures — saturating_add prevents wraparound to 0
        // (which would accidentally close the breaker). Not achievable in
        // practice but a real bug if record_failure used bare `+= 1`.
        let mut b = CacheCheckBreaker {
            consecutive_failures: u32::MAX,
            ..CacheCheckBreaker::default()
        };
        b.record_failure(); // would wrap without saturating_add
        assert_eq!(b.consecutive_failures, u32::MAX);
    }

    // r[verify sched.breaker.cache-check+2]
    /// Auto-close returns to true Closed semantics: after `OPEN_DURATION`
    /// elapses with no probe, a single failure does NOT re-trip — it
    /// takes [`OPEN_THRESHOLD`] fresh failures.
    #[test]
    fn auto_close_resets_to_closed_semantics() {
        let mut b = CacheCheckBreaker::<MockClock>::new();
        for _ in 1..=5 {
            b.record_failure();
        }
        assert!(b.is_open());
        b.clock.advance(OPEN_DURATION + Duration::from_secs(1));
        assert!(!b.is_open(), "auto-close after OPEN_DURATION");
        // 1st post-auto-close failure: under threshold, NOT a re-trip.
        assert!(
            !b.record_failure(),
            "auto-close MUST reset to Closed (5 fresh failures to re-trip)"
        );
        for i in 2..=4 {
            assert!(!b.record_failure(), "post-auto-close failure {i}");
        }
        assert!(b.record_failure(), "5th fresh failure trips again");
    }

    // r[verify sched.breaker.cache-check+2]
    /// Auto-close + single failure does NOT double-count
    /// `circuit_open_total` (observability.md: counts open transitions
    /// (outages), not 30s windows).
    #[test]
    fn auto_close_then_failure_no_duplicate_metric() {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut b = CacheCheckBreaker::<MockClock>::new();
        for _ in 1..=5 {
            b.record_failure();
        }
        assert_eq!(
            recorder.get("rio_scheduler_cache_check_circuit_open_total{}"),
            1
        );
        b.clock.advance(OPEN_DURATION + Duration::from_secs(1));
        // Single post-auto-close failure: under threshold → no metric.
        b.record_failure();
        assert_eq!(
            recorder.get("rio_scheduler_cache_check_circuit_open_total{}"),
            1,
            "auto-close + 1 failure must NOT re-increment open_total"
        );
    }

    /// `OPEN_DURATION` MUST cover the merge-time FMP timeout. If the
    /// half-open probe (same FMP call) can outlive the open window,
    /// the breaker auto-closes mid-probe and the probe's timeout
    /// resets the failure counter → only 1-in-OPEN_THRESHOLD submits
    /// is rejected during a sustained outage.
    #[test]
    fn open_duration_covers_merge_fmp_timeout() {
        assert!(
            OPEN_DURATION >= super::super::MERGE_FMP_TIMEOUT,
            "OPEN_DURATION ({OPEN_DURATION:?}) must be >= MERGE_FMP_TIMEOUT \
             ({:?}) so the half-open probe cannot outlive the open window",
            super::super::MERGE_FMP_TIMEOUT
        );
    }
}
