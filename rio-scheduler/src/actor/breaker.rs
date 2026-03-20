//! Circuit breaker for cache-check failures.
//!
//! Without this, a sustained store outage means every SubmitBuild treats
//! every derivation as a cache miss — an avalanche of unnecessary rebuilds
//! once the store comes back (or workers thrash trying to fetch inputs).

use super::*;

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
#[derive(Debug, Default)]
pub(crate) struct CacheCheckBreaker {
    /// Consecutive cache-check failures. Reset to 0 on any success.
    consecutive_failures: u32,
    /// If `Some`, the breaker is open until this instant. `None` = closed.
    open_until: Option<Instant>,
}

/// Trip open after this many consecutive failures.
const OPEN_THRESHOLD: u32 = 5;

/// Stay open for this long before auto-closing (even without a probe success).
const OPEN_DURATION: std::time::Duration = std::time::Duration::from_secs(30);

impl CacheCheckBreaker {
    /// Record a failure. Returns `true` if this failure trips the breaker open
    /// (or it was already open) — caller should reject SubmitBuild.
    ///
    /// The "already open" case matters: while open, we STILL attempt the cache
    /// check (half-open probe). A failed probe keeps us open but doesn't
    /// re-increment the open-transition metric (it's the same outage).
    pub(super) fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        // Already open: stay open. No metric (same outage, not a new trip).
        if self.is_open() {
            return true;
        }

        // Trip open on threshold crossing.
        if self.consecutive_failures >= OPEN_THRESHOLD {
            self.open_until = Some(Instant::now() + OPEN_DURATION);
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
    /// on the next `record_success()`. This keeps the check cheap (one
    /// `Instant::now()` compare) and avoids needing `&mut self` here.
    ///
    /// `pub(super)` so completion.rs can gate ContentLookup on the same
    /// breaker that merge.rs feeds with FindMissingPaths failures — same
    /// store, same unavailability signal.
    pub(super) fn is_open(&self) -> bool {
        self.open_until.is_some_and(|until| Instant::now() < until)
    }
}

/// Breaker unit tests: the state machine in isolation, no actor needed.
/// These are cheap synchronous tests covering the edge cases the
/// integration test above doesn't exercise (saturating add, auto-close
/// timeout interaction).
#[cfg(test)]
mod tests {
    use super::CacheCheckBreaker;

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
            ..Default::default()
        };
        b.record_failure(); // would wrap without saturating_add
        assert_eq!(b.consecutive_failures, u32::MAX);
    }
}
