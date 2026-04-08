//! Exponential backoff with jitter, and a shutdown-aware retry loop.
//!
//! Before this module existed there were seven hand-rolled
//! exponential-backoff implementations across the workspace using four
//! different jitter distributions, plus a `2u32.pow(n)` that panicked
//! in debug at `n≥32`. [`Backoff::duration`] is the single, NaN- and
//! overflow-safe replacement; [`retry`] is the optional loop wrapper
//! for call sites whose body fits a closure.
//!
//! **Per-site CONSTANTS stay local.** This module supplies the
//! mechanism (curve + jitter + safe arithmetic); each caller declares
//! its own `const FOO_BACKOFF: Backoff = Backoff { … }` with
//! base/mult/cap tuned to that RPC's latency profile. Don't add
//! "default" backoff constants here — there is no sensible default.

use std::future::Future;
use std::time::Duration;

use rand::Rng;

use crate::signal::Token;

/// Hard ceiling on any computed backoff. [`Duration::from_secs_f64`]
/// PANICS on `inf`/`NaN`/negative, so [`Backoff::duration`] clamps to
/// `[0, MAX_BACKOFF]` before converting. One year is far above any
/// sane retry interval — the clamp exists purely to keep a
/// misconfigured `cap = inf` (e.g., from TOML literal `"inf"`) from
/// crashing the process.
const MAX_BACKOFF: Duration = Duration::from_secs(365 * 86400);

/// Jitter distribution applied to a backoff duration.
///
/// All variants preserve `E[jittered] ≈ d` so the schedule's expected
/// total wait is unchanged; they differ only in spread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Jitter {
    /// No jitter: return `d` exactly. Use when only one client retries
    /// (no herd to desynchronize) and deterministic timing helps
    /// tests.
    None,
    /// `d × U(1-f, 1+f)`. `f` of `0.25` → ±25% spread; `f` of `0.5` →
    /// `[0.5d, 1.5d)`. Expected value is `d`. The common choice when
    /// you want bounded spread around the schedule entry.
    Proportional(f64),
    /// `U(0, d]` — "full jitter" from the AWS Architecture Blog.
    /// Expected value is `d/2`, so the curve effectively halves; in
    /// exchange the spread is maximal, which is what you want when N
    /// clients retry the SAME key (PutPath placeholder collisions).
    Full,
}

impl Jitter {
    /// Apply this jitter to `d`. Exposed so callers with a
    /// table-based schedule (not pure exponential — e.g., the FUSE
    /// fetch retry table) can share the jitter mechanism without
    /// adopting [`Backoff`].
    pub fn apply(&self, d: Duration) -> Duration {
        // mul_f64 panics on NaN/negative/overflow. The factors below
        // are all finite non-negative draws from [0, 2), and `d` is a
        // finite Duration, so `d × [0, 2)` cannot overflow Duration's
        // u64-seconds range for any `d` we'd ever compute (clamped to
        // 1yr in `Backoff::duration`).
        match *self {
            Jitter::None => d,
            Jitter::Proportional(f) => {
                // Clamp f to [0, 1]: a fraction outside that range is
                // a config error, but degrading is safer than
                // panicking on `random_range(hi < lo)` or producing a
                // negative factor.
                let f = f.clamp(0.0, 1.0);
                if f == 0.0 {
                    return d;
                }
                d.mul_f64(rand::rng().random_range((1.0 - f)..(1.0 + f)))
            }
            Jitter::Full => {
                if d.is_zero() {
                    return d;
                }
                d.mul_f64(rand::rng().random_range(0.0..=1.0))
            }
        }
    }
}

/// Exponential backoff curve: `base × multᵃ`, capped at `cap`, then
/// jittered.
///
/// All fields are `pub` so call sites can declare a `const` policy
/// inline. `Duration` and `f64` are both const-constructible.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// Delay at attempt 0 (before any multiplication).
    pub base: Duration,
    /// Per-attempt multiplier. `2.0` = doubling.
    pub mult: f64,
    /// Ceiling applied BEFORE jitter. Jitter can exceed `cap` by up
    /// to its spread (e.g., `Proportional(0.5)` → up to `1.5 × cap`);
    /// that's intentional — capping after jitter would clip the
    /// distribution and re-synchronize the herd at the cap.
    pub cap: Duration,
    /// Jitter applied last. See [`Jitter`].
    pub jitter: Jitter,
}

impl Backoff {
    /// Compute the sleep duration for retry `attempt` (0-indexed).
    ///
    /// Safe for any `attempt` and any field values: `mult.powi(BIG)`
    /// saturates to `+inf`, `inf.min(cap)` = `cap`, and the final
    /// `[0, 1yr]` clamp handles `NaN`/`inf` propagated from a
    /// misconfigured `mult`/`cap`. Never panics.
    pub fn duration(&self, attempt: u32) -> Duration {
        // Integer 2u32.pow(attempt) — the old pattern — panics in
        // debug at attempt≥32 and silently wraps in release. f64
        // powi saturates to inf instead, which the cap absorbs.
        //
        // `attempt as i32` would wrap to negative for attempt≥2³¹
        // (`u32::MAX as i32 == -1` → `mult^-1 = 1/mult`). Clamp to
        // 1<<16: any `mult>1` raised to 65536 is already inf, and
        // 65536 retries is years of wall-clock at any sane base.
        let exp = attempt.min(1 << 16) as i32;
        let raw = self.base.as_secs_f64() * self.mult.powi(exp);
        let capped = raw.min(self.cap.as_secs_f64());
        // NOT .clamp(): clamp(NaN, lo, hi) = NaN, which would then
        // panic from_secs_f64. .max(0.0) handles NaN (NaN.max(x) = x
        // per IEEE 754); .min(MAX) handles inf.
        #[allow(clippy::manual_clamp)]
        let safe = capped.max(0.0).min(MAX_BACKOFF.as_secs_f64());
        // Jitter::Proportional can multiply by up to (1+f), pushing
        // the result above `cap`. Re-clamp so the policy cap is a
        // hard ceiling regardless of jitter — matches the original
        // RetryPolicy::backoff_duration semantics.
        self.jitter
            .apply(Duration::from_secs_f64(safe))
            .min(self.cap)
            .min(MAX_BACKOFF)
    }
}

/// Terminal state of [`retry`].
#[derive(Debug)]
pub enum RetryError<E> {
    /// `shutdown` fired before success. Caller should treat this as a
    /// clean exit, not a failure to surface.
    Cancelled,
    /// `max_attempts` reached, or `is_retryable` returned `false`.
    /// `last` is the most recent error.
    Exhausted { last: E, attempts: u32 },
}

impl<E: std::fmt::Display> std::fmt::Display for RetryError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryError::Cancelled => write!(f, "retry cancelled by shutdown"),
            RetryError::Exhausted { last, attempts } => {
                write!(f, "retry exhausted after {attempts} attempts: {last}")
            }
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for RetryError<E> {}

/// Retry `op` up to `max_attempts` times with `policy` backoff between
/// attempts.
///
/// Each sleep is `select!`-raced against `shutdown.cancelled()`; the
/// token is also checked before every attempt, so a pre-cancelled
/// token short-circuits without calling `op`. `max_attempts` of
/// [`u32::MAX`] is effectively infinite — exit happens via `Ok`,
/// `Cancelled`, or `is_retryable` → `false`.
///
/// `is_retryable(&e) == false` returns `Exhausted { last: e }`
/// immediately (no sleep) — non-retryable errors should surface
/// promptly.
///
/// Most call sites in this workspace have side effects (metrics,
/// special-case branches) inside their retry loop and use
/// [`Backoff::duration`] directly instead of this wrapper. That's
/// fine — this exists for the simple cases.
pub async fn retry<T, E, F, Fut>(
    policy: &Backoff,
    max_attempts: u32,
    shutdown: &Token,
    is_retryable: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, RetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0u32;
    loop {
        if shutdown.is_cancelled() {
            return Err(RetryError::Cancelled);
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                if attempt >= max_attempts || !is_retryable(&e) {
                    return Err(RetryError::Exhausted {
                        last: e,
                        attempts: attempt,
                    });
                }
                // attempt-1: attempt is now 1 after the FIRST failure;
                // the first sleep should be `base × mult⁰ = base`.
                let delay = policy.duration(attempt - 1);
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return Err(RetryError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const P: Backoff = Backoff {
        base: Duration::from_secs(1),
        mult: 2.0,
        cap: Duration::from_secs(16),
        jitter: Jitter::None,
    };

    #[test]
    fn duration_doubles_then_caps() {
        assert_eq!(P.duration(0), Duration::from_secs(1));
        assert_eq!(P.duration(1), Duration::from_secs(2));
        assert_eq!(P.duration(3), Duration::from_secs(8));
        assert_eq!(P.duration(4), Duration::from_secs(16));
        assert_eq!(P.duration(5), Duration::from_secs(16), "capped");
        assert_eq!(
            P.duration(1000),
            Duration::from_secs(16),
            "capped at large n"
        );
    }

    /// The bug this module exists to kill: `2u32.pow(n)` panicked in
    /// debug at n≥32. f64 powi saturates → cap absorbs it.
    #[test]
    fn duration_overflow_safe_at_large_attempt() {
        assert_eq!(P.duration(u32::MAX), Duration::from_secs(16));
    }

    /// `cap = inf` (e.g., parsed from TOML literal `"inf"`) must not
    /// panic in `from_secs_f64`. The 1-year clamp catches it.
    #[test]
    fn duration_infinity_cap_clamped() {
        let p = Backoff {
            cap: Duration::MAX,
            mult: f64::INFINITY,
            ..P
        };
        let d = p.duration(100);
        assert!(d <= MAX_BACKOFF, "inf clamped to 1yr, got {d:?}");
    }

    /// `mult = NaN` propagates through `powi` → `raw = NaN` →
    /// `NaN.min(cap) = cap` (Rust's `f64::min` returns the non-NaN
    /// operand). Safety property is "never panics, finite result";
    /// degrading to `cap` is the right answer for garbage config.
    #[test]
    fn duration_nan_mult_degrades_to_cap() {
        let p = Backoff {
            mult: f64::NAN,
            ..P
        };
        assert_eq!(p.duration(1), P.cap);
    }

    #[test]
    fn jitter_proportional_bounds_and_variance() {
        let base = Duration::from_secs(4);
        let j = Jitter::Proportional(0.25);
        let lo = base.mul_f64(0.75);
        let hi = base.mul_f64(1.25);
        let samples: Vec<_> = (0..200).map(|_| j.apply(base)).collect();
        for s in &samples {
            assert!(*s >= lo && *s <= hi, "{s:?} outside [{lo:?}, {hi:?}]");
        }
        assert!(
            samples.iter().any(|s| *s != samples[0]),
            "Proportional jitter produced 200 identical samples"
        );
    }

    #[test]
    fn jitter_proportional_clamps_fraction() {
        // f > 1.0 would make (1-f) negative → mul_f64 panic. Clamp
        // degrades it to f=1.0 → factor in [0, 2).
        let d = Jitter::Proportional(5.0).apply(Duration::from_secs(1));
        assert!(d <= Duration::from_secs(2));
    }

    #[test]
    fn jitter_full_bounds() {
        let base = Duration::from_secs(4);
        for _ in 0..200 {
            let d = Jitter::Full.apply(base);
            assert!(d <= base, "{d:?} > base {base:?}");
        }
        assert_eq!(Jitter::Full.apply(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_cancelled_during_sleep() {
        let token = Token::new();
        let calls = Arc::new(AtomicU32::new(0));

        let c = Arc::clone(&calls);
        let t = token.clone();
        let task = tokio::spawn(async move {
            retry(
                &P,
                u32::MAX,
                &t,
                |_: &&str| true,
                || {
                    let c = Arc::clone(&c);
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Err::<(), _>("nope")
                    }
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        token.cancel();
        tokio::task::yield_now().await;
        assert!(matches!(task.await.unwrap(), Err(RetryError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry after cancel");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_at_max_attempts() {
        let token = Token::new();
        let calls = AtomicU32::new(0);
        let r: Result<(), _> = retry(
            &P,
            3,
            &token,
            |_: &&str| true,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("nope") }
            },
        )
        .await;
        assert!(matches!(
            r,
            Err(RetryError::Exhausted {
                last: "nope",
                attempts: 3
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_non_retryable_surfaces_immediately() {
        let token = Token::new();
        let calls = AtomicU32::new(0);
        let r: Result<(), _> = retry(
            &P,
            10,
            &token,
            |e: &&str| *e != "fatal",
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("fatal") }
            },
        )
        .await;
        assert!(matches!(r, Err(RetryError::Exhausted { attempts: 1, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_eventual_success() {
        let token = Token::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let r = retry(
            &P,
            10,
            &token,
            |_: &&str| true,
            move || {
                let c = Arc::clone(&c);
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 { Err("not yet") } else { Ok(n) }
                }
            },
        )
        .await;
        assert_eq!(r.unwrap(), 3);
    }
}
