//! [`RecoveredInstant`] — a monotonic anchor that survives failover
//! arithmetic without the silent re-anchor (`merged_bug_300`).
//!
//! The bug class: recovery reconstructs "when did X begin" from a
//! PG-computed elapsed age via `Instant::now().checked_sub(age)`. On a
//! node booted more recently than the event, `checked_sub` returns
//! `None` and the `unwrap_or(now)` fallback silently RESETS the clock —
//! a 30-hour-old build reads as submitted-now (a fresh full
//! `build_timeout` window), a parked job's dwell clock restarts, a
//! poison TTL extends. The failure is invisible: no log, no counter,
//! just a quietly wrong clock.
//!
//! The fix is representational: carry the recovered age as DATA next to
//! a fresh anchor instead of forcing it into a pre-boot `Instant` that
//! may not exist. `elapsed()` is then total — `recovered_at.elapsed() +
//! age_at_recovery` — and a pre-boot moment is exactly representable.
//! No fallback arm exists to take.
//!
//! The `nix/misc-checks.nix` `no-preboot-instant` policy bans the
//! `checked_sub(..).unwrap_or*(Instant::now..)` idiom across `rio-*/src`
//! (zero allowlist); this type is the sanctioned replacement at every
//! recovery anchor.

use std::time::{Duration, Instant};

/// A point in time that may pre-date process boot, recovered from a
/// durable wall-clock age. Use [`Self::fresh_now`] at live sites and
/// [`Self::from_age_secs`] at recovery sites; consumers read
/// [`Self::elapsed`] exactly like `Instant::elapsed`.
#[derive(Debug, Clone, Copy)]
pub struct RecoveredInstant {
    /// How old the moment already was when this process anchored it.
    age_at_recovery: Duration,
    /// The local monotonic anchor laid down at recovery (or at the live
    /// event, with zero age).
    recovered_at: Instant,
}

impl RecoveredInstant {
    /// A live event happening right now (age zero).
    pub fn fresh_now() -> Self {
        Self {
            age_at_recovery: Duration::ZERO,
            recovered_at: Instant::now(),
        }
    }

    /// Recover a moment that was `age_secs` old at the time of this
    /// call (the PG `EXTRACT(EPOCH FROM now() - t)` shape). Clamps:
    /// negative/NaN → 0 (a fresh event — conservative for TTL-style
    /// consumers), +inf/absurd → one year (the
    /// `from_poisoned_row` precedent: `-infinity::timestamp` must not
    /// panic `from_secs_f64`).
    pub fn from_age_secs(age_secs: f64) -> Self {
        // merged_bug_262: the precedent absorbed into its
        // generalization -- the clamp semantics live in
        // rio_common::clamped (NaN/neg -> 0, +inf/absurd -> 1yr,
        // byte-identical to the old inline clamp).
        Self {
            age_at_recovery: rio_common::clamped::clamped_duration_secs(age_secs),
            recovered_at: Instant::now(),
        }
    }

    /// Total time since the (possibly pre-boot) moment: the local
    /// anchor's elapsed plus the recovered age. Never re-anchors.
    pub fn elapsed(&self) -> Duration {
        self.recovered_at.elapsed() + self.age_at_recovery
    }

    /// Test/debug seeding: a moment `secs_ago` seconds in the past,
    /// regardless of process uptime (tokio paused time cannot mock
    /// `Instant`; this is the `DebugBackdate*` mechanism).
    pub fn backdated(secs_ago: u64) -> Self {
        Self {
            age_at_recovery: Duration::from_secs(secs_ago),
            recovered_at: Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_age_elapsed_is_at_least_the_age_immediately() {
        // The 300 red shape: a 30-hour-old moment must read ≥ 30h
        // immediately, even though no Instant 30h ago exists for a
        // fresh process.
        let thirty_hours = 30.0 * 3600.0;
        let r = RecoveredInstant::from_age_secs(thirty_hours);
        assert!(
            r.elapsed() >= rio_common::clamped::clamped_duration_secs(thirty_hours),
            "recovered elapsed must include the pre-recovery age"
        );
    }

    #[test]
    fn clamps_are_total() {
        assert!(RecoveredInstant::from_age_secs(f64::NAN).elapsed() < Duration::from_secs(60));
        assert!(RecoveredInstant::from_age_secs(-5.0).elapsed() < Duration::from_secs(60));
        // +inf clamps to a year instead of panicking from_secs_f64.
        let r = RecoveredInstant::from_age_secs(f64::INFINITY);
        assert!(r.elapsed() >= Duration::from_secs(364 * 86400));
    }

    #[test]
    fn fresh_now_is_young() {
        assert!(RecoveredInstant::fresh_now().elapsed() < Duration::from_secs(60));
    }
}
