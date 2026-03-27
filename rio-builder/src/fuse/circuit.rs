//! FUSE fetch circuit breaker. **std::sync ONLY** — FUSE callbacks run on
//! fuser's thread pool, NOT in a tokio context (see fetch.rs SYNC comment
//! on `fetch_extract_insert`). No tokio primitives; no `.await`.
//!
//! Two trip conditions — EITHER opens the circuit:
//!   (a) `threshold` (default 5) consecutive `ensure_cached` fetch failures
//!   (b) `last_success.elapsed() > wall_clock_trip` (default 90s) AND at
//!       least one failure since last success — catches the degraded-but-
//!       alive store (accepting connections, serving slowly) without waiting
//!       for threshold × fetch_timeout. The failure-gate is critical: an
//!       idle build (e.g., a 180s sleep with no store traffic) would
//!       otherwise trip the breaker on its first post-sleep fetch, turning
//!       a quiescent worker into a false-positive `store_degraded`.
//!
//! After `auto_close_after` (default 30s) the circuit goes half-open:
//! [`check`](CircuitBreaker::check) returns `Ok` to let ONE fetch probe.
//! The probe's [`record`](CircuitBreaker::record) call closes on success,
//! re-opens (fresh 30s timer) on failure.
//!
//! Pattern reference: `rio-scheduler/src/actor/breaker.rs` has the same
//! 3-state shape, but that one is single-threaded-actor (plain `u32`);
//! this one needs atomics because fuser's thread pool is multi-threaded.
//
// r[impl worker.fuse.circuit-breaker+2]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use fuser::Errno;

// ── Clock trait: testability without real sleeps ──────────────────────
// tokio::time::pause does NOT work here — this is std::time::Instant,
// not tokio's monotonic clock. Inject via trait so tests can advance
// a MockClock instead of sleeping.

/// Time source for the breaker. Production uses [`SystemClock`];
/// tests inject a mock so the state-transition tests don't need
/// real 30s/90s sleeps.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock: `Instant::now()`.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

// ── CircuitBreaker ────────────────────────────────────────────────────

/// Open after this many consecutive fetch failures.
const DEFAULT_THRESHOLD: u32 = 5;
/// Half-open this long after opening (let one probe through).
const DEFAULT_AUTO_CLOSE: Duration = Duration::from_secs(30);
/// Open if no successful fetch for this long (degraded-store trip).
const DEFAULT_WALL_CLOCK_TRIP: Duration = Duration::from_secs(90);

/// Three-state circuit breaker for the FUSE fetch path.
///
/// Shared across all fuser threads (`Arc<CircuitBreaker>` on
/// `NixStoreFs`). All methods take `&self` — interior mutability
/// via `AtomicU32` + `std::sync::Mutex` (not tokio's mutex).
pub struct CircuitBreaker<C: Clock = SystemClock> {
    /// Consecutive fetch failures. Reset to 0 on any success. Relaxed
    /// ordering is sufficient — precise interleaving at the threshold
    /// boundary doesn't matter; the `open_since` Mutex serializes the
    /// actual open transition.
    consecutive_failures: AtomicU32,

    /// If `Some(t)`, circuit opened at `t`. Open until `t + auto_close_after`,
    /// then half-open (next check() probes). `None` = closed. Cleared
    /// lazily on `record(true)`, not eagerly on half-open transition —
    /// a stale `Some` with elapsed > auto_close is indistinguishable from
    /// `None` via `is_open()`.
    open_since: Mutex<Option<Instant>>,

    /// Last successful fetch. `None` until first success — the wall-clock
    /// trip can't fire on a fresh worker that's served only cache hits for
    /// 90s (it would otherwise open the circuit without ever having tried
    /// the store). Flips to `Some` on first `record(true)`.
    last_success: Mutex<Option<Instant>>,

    threshold: u32,
    auto_close_after: Duration,
    wall_clock_trip: Duration,
    clock: C,
}

impl Default for CircuitBreaker<SystemClock> {
    fn default() -> Self {
        Self::with_clock(
            DEFAULT_THRESHOLD,
            DEFAULT_AUTO_CLOSE,
            DEFAULT_WALL_CLOCK_TRIP,
            SystemClock,
        )
    }
}

impl<C: Clock> CircuitBreaker<C> {
    pub fn with_clock(
        threshold: u32,
        auto_close_after: Duration,
        wall_clock_trip: Duration,
        clock: C,
    ) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            open_since: Mutex::new(None),
            last_success: Mutex::new(None),
            threshold,
            auto_close_after,
            wall_clock_trip,
            clock,
        }
    }

    /// `Err(EIO)` if open. `Ok` if closed or half-open.
    ///
    /// Called immediately before a remote fetch attempt (NOT before
    /// cache-hit fast-path — cache hits don't touch the store). If this
    /// returns `Err`, fail the FUSE op with `EIO` and skip the fetch.
    ///
    /// Pure sync — no `.await`.
    pub fn check(&self) -> Result<(), Errno> {
        let now = self.clock.now();

        // ── Trip (a) already fired: explicit open ─────────────────────
        {
            let guard = self.open_since.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(since) = *guard {
                if now.duration_since(since) < self.auto_close_after {
                    return Err(Errno::EIO); // open — fail fast
                }
                // Half-open: auto_close_after elapsed. Return Ok to let
                // the probe fetch through. DON'T fall through to the
                // wall-clock check — last_success hasn't been updated
                // since before we opened, so it would almost certainly
                // re-trip and starve the probe. Half-open overrides.
                return Ok(());
            }
        }

        // ── Trip (b): degraded-store wall-clock check ─────────────────
        // Only fires after at least one prior success (last_success
        // Some) AND at least one failure since (consecutive_failures >0).
        // The failure-gate distinguishes degraded from IDLE: a build that
        // sleeps 180s with zero fetch attempts has a stale last_success
        // but a healthy store. Without the gate, the first post-idle
        // check() trips → EIO on the upload read → InfrastructureFailure
        // → scheduler reassign loop. Observed: smoke-test step 7's
        // `read -t 180` build restarted 6× before timeout.
        let last = *self.last_success.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = last
            && now.duration_since(t) > self.wall_clock_trip
            && self.consecutive_failures.load(Ordering::Relaxed) > 0
        {
            let mut guard = self.open_since.lock().unwrap_or_else(|e| e.into_inner());
            // Re-check under lock: another thread may have opened.
            if guard.is_none() {
                *guard = Some(now);
                metrics::gauge!("rio_builder_fuse_circuit_open").set(1.0);
                tracing::warn!(
                    since_last_success = ?now.duration_since(t),
                    wall_clock_trip = ?self.wall_clock_trip,
                    "FUSE circuit breaker OPENING (wall-clock trip — store degraded)"
                );
            }
            return Err(Errno::EIO);
        }

        Ok(()) // closed
    }

    /// Record a fetch result. `true` → reset + close. `false` → increment;
    /// maybe open on threshold.
    ///
    /// Called immediately after a remote fetch attempt returns (inside
    /// the `FetchClaim::Fetch` arm — the `WaitFor` arm does NOT record;
    /// the fetching thread does).
    ///
    /// Pure sync — no `.await`.
    pub fn record(&self, ok: bool) {
        if ok {
            self.consecutive_failures.store(0, Ordering::Relaxed);
            *self.last_success.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.clock.now());
            // take(): clear + return whether we WERE open, atomically.
            let was_open = self
                .open_since
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .is_some();
            if was_open {
                metrics::gauge!("rio_builder_fuse_circuit_open").set(0.0);
                tracing::info!("FUSE circuit breaker CLOSING — store recovered");
            }
        } else {
            // fetch_add returns the PREVIOUS value; +1 for the new count.
            // Overflow at u32::MAX wraps the atomic (next fetch_add
            // returns 0) but that's 4 billion consecutive failures —
            // not reachable in practice.
            let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= self.threshold {
                let now = self.clock.now();
                let mut guard = self.open_since.lock().unwrap_or_else(|e| e.into_inner());
                // Refresh open_since UNCONDITIONALLY:
                //   closed    → opens (guard: None → Some(now))
                //   open      → stays open (timer refresh is harmless)
                //   half-open → RE-opens with FRESH 30s timer (stale
                //               Some → fresh Some) — the probe failed
                // The `already_open` check gates the log/metric (one per
                // transition, not one per failure-while-open), not state.
                let already_open =
                    guard.is_some_and(|t| now.duration_since(t) < self.auto_close_after);
                *guard = Some(now);
                if !already_open {
                    metrics::gauge!("rio_builder_fuse_circuit_open").set(1.0);
                    tracing::warn!(
                        consecutive_failures = n,
                        threshold = self.threshold,
                        auto_close_after = ?self.auto_close_after,
                        "FUSE circuit breaker OPENING (failure threshold)"
                    );
                }
            }
        }
    }

    /// Whether the breaker is open RIGHT NOW. Half-open counts as NOT
    /// open (the probe is allowed). P0210's heartbeat reports this.
    ///
    /// Doesn't mutate — the stale `open_since` is cleaned up lazily on
    /// the next `record(true)`.
    pub fn is_open(&self) -> bool {
        self.open_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|since| self.clock.now().duration_since(since) < self.auto_close_after)
    }
}

// ══════════════════════════════════════════════════════════════════════
// Tests — plain #[test], NOT #[tokio::test]. This is std::sync; tokio's
// time::pause doesn't apply. MockClock lets us advance time manually.
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock clock: tests mutate `.0` directly via `advance()`.
    struct MockClock(Mutex<Instant>);
    impl Clock for MockClock {
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }
    impl MockClock {
        fn new() -> Self {
            Self(Mutex::new(Instant::now()))
        }
        fn advance(&self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }
    }

    /// 5/30s/90s, matching production defaults. Clock owned by the breaker;
    /// tests advance it via `b.clock.advance(...)`.
    fn breaker() -> CircuitBreaker<MockClock> {
        CircuitBreaker::with_clock(
            DEFAULT_THRESHOLD,
            DEFAULT_AUTO_CLOSE,
            DEFAULT_WALL_CLOCK_TRIP,
            MockClock::new(),
        )
    }

    // ── Trip condition (a): consecutive-failure threshold ─────────────

    // r[verify worker.fuse.circuit-breaker+2]
    #[test]
    fn four_failures_stay_closed() {
        let b = breaker();
        for i in 1..=4 {
            b.record(false);
            assert!(b.check().is_ok(), "failure {i}/4: still closed");
            assert!(!b.is_open());
        }
    }

    #[test]
    fn fifth_failure_opens() {
        let b = breaker();
        for _ in 1..=4 {
            b.record(false);
        }
        assert!(b.check().is_ok(), "4 failures: still closed");
        b.record(false); // 5th
        assert!(b.is_open(), "5th failure: open");
        let err = b.check().expect_err("open circuit → Err");
        assert_eq!(err.code(), Errno::EIO.code());
    }

    #[test]
    fn success_resets_counter() {
        let b = breaker();
        for _ in 1..=4 {
            b.record(false);
        }
        b.record(true); // reset to 0
        // Need 5 MORE failures now, not 1.
        for i in 1..=4 {
            b.record(false);
            assert!(b.check().is_ok(), "post-reset failure {i}: still closed");
        }
        b.record(false);
        assert!(b.is_open(), "post-reset 5th: opens");
    }

    // ── Half-open: closed→open→half-open→CLOSED ───────────────────────

    #[test]
    fn half_open_after_auto_close() {
        let b = breaker();
        for _ in 1..=5 {
            b.record(false);
        }
        assert!(b.check().is_err(), "open immediately after threshold");

        b.clock.advance(Duration::from_secs(29));
        assert!(b.check().is_err(), "29s < 30s: still open");

        b.clock.advance(Duration::from_secs(2)); // total 31s
        assert!(b.check().is_ok(), "31s > 30s: half-open probe allowed");
        assert!(!b.is_open(), "half-open reports NOT open");
    }

    #[test]
    fn half_open_success_closes() {
        let b = breaker();
        for _ in 1..=5 {
            b.record(false);
        }
        b.clock.advance(Duration::from_secs(31));
        assert!(b.check().is_ok(), "half-open: probe allowed");

        b.record(true); // probe succeeded
        assert!(!b.is_open(), "closed after probe success");
        assert!(b.check().is_ok(), "closed: check passes");

        // Counter fully reset: need 5 more to re-open.
        for i in 1..=4 {
            b.record(false);
            assert!(b.check().is_ok(), "post-close failure {i}: closed");
        }
    }

    // ── Half-open: closed→open→half-open→OPEN ─────────────────────────

    #[test]
    fn half_open_failure_reopens() {
        let b = breaker();
        for _ in 1..=5 {
            b.record(false);
        }
        b.clock.advance(Duration::from_secs(31));
        assert!(b.check().is_ok(), "half-open: probe allowed");

        b.record(false); // probe failed — counter now 6 ≥ 5 → re-open
        assert!(b.is_open(), "re-opened after failed probe");
        let err = b.check().expect_err("re-open → Err");
        assert_eq!(err.code(), Errno::EIO.code());

        // The 30s timer restarted from the probe failure, not from the
        // original open. 29s after the probe → still open.
        b.clock.advance(Duration::from_secs(29));
        assert!(b.check().is_err(), "29s after re-open: still open");
        b.clock.advance(Duration::from_secs(2));
        assert!(b.check().is_ok(), "31s after re-open: half-open again");
    }

    // ── Trip condition (b): wall-clock trip ───────────────────────────

    #[test]
    fn wall_clock_trip_needs_prior_success() {
        let b = breaker();
        // Never recorded a success → last_success is None → wall-clock
        // trip can't fire. Advance way past 90s; still closed.
        b.clock.advance(Duration::from_secs(1000));
        assert!(b.check().is_ok(), "no prior success: can't wall-clock-trip");
        assert!(!b.is_open());
    }

    #[test]
    fn wall_clock_trip_opens_after_prior_success() {
        let b = breaker();
        b.record(true); // last_success = now
        b.record(false); // arm the wall-clock: ≥1 failure since success
        b.clock.advance(Duration::from_secs(89));
        assert!(b.check().is_ok(), "89s: under wall_clock_trip");

        b.clock.advance(Duration::from_secs(2)); // 91s
        let err = b.check().expect_err("91s > 90s: wall-clock trip");
        assert_eq!(err.code(), Errno::EIO.code());
        assert!(b.is_open(), "wall-clock trip sets open_since");
    }

    /// Regression: smoke-test step 7's `read -t 180` build. A build that
    /// sleeps with no store traffic has a stale `last_success` but the
    /// store is NOT degraded — it's idle. The first post-sleep fetch
    /// must not trip. Gate: `consecutive_failures > 0`.
    #[test]
    fn wall_clock_trip_idle_no_failures_stays_closed() {
        let b = breaker();
        b.record(true); // last_success = now
        // No failures recorded — the build is sleeping, not fetching.
        b.clock.advance(Duration::from_secs(180));
        assert!(
            b.check().is_ok(),
            "180s idle with 0 failures: store is idle, not degraded"
        );
        assert!(!b.is_open());

        // A fetch goes through and succeeds — refreshes last_success.
        b.record(true);
        b.clock.advance(Duration::from_secs(50));
        assert!(b.check().is_ok(), "50s after refresh: closed");
    }

    /// The wall-clock gate disarms on the next success: success →
    /// failure (arms) → success (disarms) → idle 91s → no trip.
    #[test]
    fn wall_clock_trip_disarmed_by_intervening_success() {
        let b = breaker();
        b.record(true);
        b.record(false); // arms
        b.record(true); // disarms (resets consecutive_failures)
        b.clock.advance(Duration::from_secs(91));
        assert!(
            b.check().is_ok(),
            "failure→success disarms the gate; idle 91s is still idle"
        );
    }

    #[test]
    fn wall_clock_trip_then_probe_success_closes() {
        let b = breaker();
        b.record(true);
        b.record(false); // arm the wall-clock gate
        b.clock.advance(Duration::from_secs(91));
        assert!(b.check().is_err(), "wall-clock trip");

        // Half-open after 30s. The half-open override in check() means
        // the probe gets through even though last_success is still 121s
        // old (91+30) — otherwise the wall-clock check would starve it.
        b.clock.advance(Duration::from_secs(31));
        assert!(
            b.check().is_ok(),
            "half-open overrides stale wall-clock check"
        );

        b.record(true); // probe succeeded — updates last_success
        assert!(!b.is_open());
        assert!(b.check().is_ok(), "fresh last_success: closed");
    }

    #[test]
    fn success_refreshes_wall_clock() {
        let b = breaker();
        b.record(true);
        b.clock.advance(Duration::from_secs(80));
        b.record(true); // refresh last_success
        b.clock.advance(Duration::from_secs(80)); // 160s total, but 80s since refresh
        assert!(b.check().is_ok(), "80s since last success: closed");
    }
}
