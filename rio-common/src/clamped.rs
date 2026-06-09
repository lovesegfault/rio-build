//! One clamped constructor for externally-sourced seconds → `Duration`
//! (bughunt-4 merged_bug_262).
//!
//! `Duration::from_secs_f64` PANICS on +inf/NaN/negative — and
//! PG-derived `EXTRACT(EPOCH ...)::float8` values, TOML config floats,
//! and wire-carried f64s can all carry every one of those shapes
//! (`'infinity'::timestamptz` is valid SQL; `inf` is valid TOML). A
//! half-guard (`.max(0.0)` / `> 0.0`) stops NaN and negatives but
//! passes +inf straight into the panic, which is how one poisoned row
//! crash-looped the leader recovery rebuild on every candidate.
//!
//! The workspace ban on raw `from_secs_f64` (clippy `disallowed-methods`)
//! is the machine witness: every construction routes here or fails
//! `--deny warnings`. The clamp generalizes the
//! `RecoveredInstant::from_age_secs` precedent verbatim: NaN/negative →
//! zero (a fresh/empty value — conservative for TTL-style consumers),
//! +inf/absurd → one year.

use std::time::{Duration, SystemTime};

/// A `Duration` built from an untrusted f64 seconds value through the
/// one total constructor. Cannot panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClampedSecs(Duration);

impl ClampedSecs {
    /// The absurdity ceiling: one year, the `from_poisoned_row`
    /// precedent.
    pub const MAX_SECS: f64 = 365.0 * 86400.0;

    /// Total construction: NaN/negative → 0, +inf/>1yr → one year.
    pub fn from_f64(secs: f64) -> Self {
        // NaN.max(x) = x, so the .max also launders NaN to 0.
        #[allow(clippy::manual_clamp)]
        let clamped = secs.max(0.0).min(Self::MAX_SECS);
        // The ONE sanctioned raw call in the workspace: the input is
        // provably in [0, MAX_SECS] here.
        #[allow(clippy::disallowed_methods)]
        Self(Duration::from_secs_f64(clamped))
    }

    /// The clamped duration.
    pub fn duration(self) -> Duration {
        self.0
    }

    /// Whether the clamp floored the value to zero (NaN/negative/zero
    /// input) — callers that treat "no value" specially read this
    /// instead of re-deriving the predicate.
    pub fn is_zero(self) -> bool {
        self.0 == Duration::ZERO
    }
}

/// Convenience: clamped seconds straight to `Duration`.
pub fn clamped_duration_secs(secs: f64) -> Duration {
    ClampedSecs::from_f64(secs).duration()
}

/// Total constructor for ABSOLUTE epoch seconds → `SystemTime`: the
/// EPOCH-domain twin of [`ClampedSecs`] (the AGE/interval clamp).
///
/// The two domains need two constructors because the age clamp's
/// 1-year absurdity ceiling is *meaningless* for absolute timestamps —
/// routing an epoch through it relocates every real timestamp to 1971
/// (the bughunt-4 sibling sweep found exactly that distortion at two
/// sites, after the controller sketch precedent had already named it).
/// Pick by domain:
///
/// - seconds **since** something (age, interval, backoff, TTL) →
///   [`ClampedSecs`] / [`clamped_duration_secs`],
/// - seconds **at** something (`EXTRACT(EPOCH FROM <timestamptz>)`,
///   wire-carried epoch stamps) → this function.
///
/// Poisoned input (NaN / negative / ±inf / past-`SystemTime`-range) →
/// `None`. The caller picks the refusal shape: optional wire fields
/// stay absent; mandatory states warn and reset to `UNIX_EPOCH` (the
/// sketch re-warm precedent — recoverable where the panic was not).
pub fn epoch_secs(secs: f64) -> Option<SystemTime> {
    // The ONE sanctioned `try_from_secs_f64` call in the workspace
    // (clippy `disallowed-methods` routes every other site here):
    // totality is the point — Err on NaN/negative/non-finite/overflow
    // — and NO ceiling, because the domain is absolute.
    #[allow(clippy::disallowed_methods)]
    let d = Duration::try_from_secs_f64(secs).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Totality over the full adversarial alphabet: the constructor
    /// never panics and lands in [0, 1yr].
    #[test]
    fn clamp_is_total() {
        for (input, expect_secs) in [
            (f64::INFINITY, ClampedSecs::MAX_SECS),
            (f64::NEG_INFINITY, 0.0),
            (f64::NAN, 0.0),
            (-1.0, 0.0),
            (0.0, 0.0),
            (1.5, 1.5),
            (1e18, ClampedSecs::MAX_SECS),
        ] {
            let got = ClampedSecs::from_f64(input).duration();
            assert!(
                (got.as_secs_f64() - expect_secs).abs() < 1e-9,
                "from_f64({input}) -> {got:?}, expected {expect_secs}s"
            );
        }
        assert!(ClampedSecs::from_f64(f64::NAN).is_zero());
        assert!(!ClampedSecs::from_f64(1.0).is_zero());
    }

    /// The EPOCH constructor: total over the adversarial alphabet and
    /// — the property the AGE clamp cannot give — undistorted on real
    /// timestamps. The first assertion documents the defect class this
    /// kills: a 2023 epoch through the age clamp lands at the 1-year
    /// ceiling (1971).
    #[test]
    fn epoch_constructor_is_total_and_undistorted() {
        let real_epoch = 1.7e9; // 2023-11-14T22:13:20Z
        let through_age_clamp = SystemTime::UNIX_EPOCH + clamped_duration_secs(real_epoch);
        assert_eq!(
            through_age_clamp,
            SystemTime::UNIX_EPOCH + Duration::from_secs(ClampedSecs::MAX_SECS as u64),
            "the age clamp relocates real epochs to 1971 — epoch sites use epoch_secs"
        );
        assert_eq!(
            epoch_secs(real_epoch),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "epoch_secs must carry the real timestamp undistorted"
        );
        assert_eq!(epoch_secs(0.0), Some(SystemTime::UNIX_EPOCH));
        for poisoned in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1.9e19] {
            assert_eq!(
                epoch_secs(poisoned),
                None,
                "poisoned epoch {poisoned} must refuse, not panic or fabricate"
            );
        }
    }
}
