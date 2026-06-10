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

/// The daemon-default build timeout in seconds (2 h), shared so the
/// scheduler's token-expiry fallback and the builder's
/// `DEFAULT_DAEMON_TIMEOUT` derive from ONE symbol instead of
/// mirrored `7200` literals (R14; merged_bug_034 sweep — the
/// scheduler side previously carried "Can't reference the const
/// cross-crate, so duplicate the value").
pub const DAEMON_DEFAULT_TIMEOUT_SECS: u64 = 7200;

/// Integer-seconds twin of [`ClampedSecs`] for WIRE-supplied `u64`
/// seconds (merged_bug_034): tenant-supplied scalars are bounds-typed
/// at the trust seam. `SubmitBuildRequest.{build_timeout,
/// max_silent_time}` previously crossed the proto→domain seam raw;
/// `u64::MAX` survived the scheduler's min-nonzero fold (min over one
/// element) and the builder's verbatim `Duration::from_secs`, then
/// PANICKED the stderr loop's `Instant + Duration` deadline math —
/// caught and MISCLASSIFIED as infrastructure failure.
///
/// Semantics: `0` means UNSET (no timeout) and is preserved exactly;
/// any value above the ONE shared absurdity ceiling
/// ([`ClampedSecs::MAX_SECS`], one year) SATURATES to the ceiling
/// with a debug log (Q-S8-B, signed: saturate, not reject — preserves
/// the fold's permissiveness; a `u64::MAX` submission becomes
/// effectively-unbounded-but-arithmetic-safe). Serde round-trips as
/// the raw `u64` (`from`/`into`), so JSONB-persisted rows are
/// byte-compatible AND re-clamped on load — a poisoned pre-fix row
/// saturates at read time.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(from = "u64", into = "u64")]
pub struct WireSecs(u64);

impl WireSecs {
    /// The integer view of the ONE absurdity ceiling — bound to
    /// [`ClampedSecs::MAX_SECS`] (a unit test pins the equality).
    pub const MAX_SECS: u64 = ClampedSecs::MAX_SECS as u64;

    /// The unset value (`0` on the wire = no timeout).
    pub const UNSET: WireSecs = WireSecs(0);

    /// Total mint at a proto→domain seam: saturates above the ceiling
    /// (with a debug log — the Q-S8-B signed posture), zero stays
    /// unset. This is the ONLY constructor; a raw field assignment
    /// does not compile, so an unclamped value is unrepresentable
    /// (R14: the closure set is the field type).
    pub fn from_wire(raw: u64) -> Self {
        if raw > Self::MAX_SECS {
            tracing::debug!(
                raw,
                ceiling = Self::MAX_SECS,
                "wire-supplied seconds saturated at the shared absurdity ceiling"
            );
            Self(Self::MAX_SECS)
        } else {
            Self(raw)
        }
    }

    /// Whether the wire value was `0` (= unset/no-timeout).
    #[must_use]
    pub const fn is_unset(self) -> bool {
        self.0 == 0
    }

    /// The bounded raw seconds (for proto re-emission and display).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// `Some(Duration)` for a set value, `None` for unset — the
    /// 0-means-unset wire semantics as a typed `Option`. The result
    /// is ≤ one year, so `Instant + duration` is in-range.
    #[must_use]
    pub const fn to_duration_nonzero(self) -> Option<Duration> {
        if self.0 == 0 {
            None
        } else {
            Some(Duration::from_secs(self.0))
        }
    }

    /// The min-nonzero fold law owned by the type: unset loses to any
    /// set value; two set values take the minimum. (Previously a free
    /// fn beside the scheduler fold; owning it here keeps the law and
    /// the bound in one place.)
    #[must_use]
    pub const fn min_permissive(self, other: Self) -> Self {
        match (self.0, other.0) {
            (0, _) => other,
            (_, 0) => self,
            (a, b) => {
                if a <= b {
                    self
                } else {
                    other
                }
            }
        }
    }
}

impl From<u64> for WireSecs {
    /// Serde/wire entry: identical to [`WireSecs::from_wire`] (total,
    /// saturating) — deserialized rows re-clamp on load.
    fn from(raw: u64) -> Self {
        Self::from_wire(raw)
    }
}

impl From<WireSecs> for u64 {
    fn from(w: WireSecs) -> u64 {
        w.raw()
    }
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

    /// WireSecs totality over the wire domain: zero stays unset, the
    /// ceiling is the ONE shared ceiling, everything above saturates
    /// (u64::MAX included — the merged_bug_034 panic input).
    #[test]
    fn wire_secs_mint_is_total_and_saturating() {
        assert_eq!(
            WireSecs::MAX_SECS,
            ClampedSecs::MAX_SECS as u64,
            "ONE ceiling governs all clamped seconds"
        );
        for (input, expect) in [
            (0, 0),
            (1, 1),
            (WireSecs::MAX_SECS, WireSecs::MAX_SECS),
            (WireSecs::MAX_SECS + 1, WireSecs::MAX_SECS),
            (u64::MAX, WireSecs::MAX_SECS),
        ] {
            assert_eq!(
                WireSecs::from_wire(input).raw(),
                expect,
                "from_wire({input})"
            );
        }
        assert!(WireSecs::from_wire(0).is_unset());
        assert_eq!(WireSecs::from_wire(0).to_duration_nonzero(), None);
        assert_eq!(
            WireSecs::from_wire(u64::MAX).to_duration_nonzero(),
            Some(Duration::from_secs(WireSecs::MAX_SECS)),
            "the saturated duration is Instant-add-safe"
        );
    }

    /// The fold law on the type: unset loses to any set value, two
    /// set values take the min — the scheduler's previous free-fn
    /// `min_nonzero` semantics, exactly.
    #[test]
    fn wire_secs_min_permissive_fold_law() {
        let unset = WireSecs::UNSET;
        let five = WireSecs::from_wire(5);
        let nine = WireSecs::from_wire(9);
        assert_eq!(unset.min_permissive(five), five);
        assert_eq!(five.min_permissive(unset), five);
        assert_eq!(five.min_permissive(nine), five);
        assert_eq!(nine.min_permissive(five), five);
        assert_eq!(unset.min_permissive(unset), unset);
    }

    /// Serde: raw-u64 wire/JSONB compatibility, and — the defense in
    /// depth — re-clamp on load: a row persisted PRE-fix with
    /// u64::MAX deserializes saturated.
    #[test]
    fn wire_secs_serde_roundtrips_and_reclamps() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Row {
            t: WireSecs,
        }
        let json = serde_json::to_string(&Row {
            t: WireSecs::from_wire(120),
        })
        .unwrap();
        assert_eq!(json, r#"{"t":120}"#, "wire format is the bare integer");
        let back: Row = serde_json::from_str(&json).unwrap();
        assert_eq!(back.t.raw(), 120);

        let poisoned: Row = serde_json::from_str(r#"{"t":18446744073709551615}"#).unwrap();
        assert_eq!(
            poisoned.t.raw(),
            WireSecs::MAX_SECS,
            "a pre-fix JSONB row carrying u64::MAX must saturate on load"
        );
    }
}
