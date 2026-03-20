//! Per-tenant build-submit rate limiting.
//!
//! Keyed on `tenant_name` (the authorized_keys comment — see
//! `server.rs` `auth_publickey`). The keyspace is operator-controlled
//! (each key line is written by an operator, not the client), so no
//! eviction is needed: `governor::DefaultKeyedRateLimiter` uses
//! `dashmap` with no automatic cleanup, which is fine for a
//! bounded-by-config keyspace. Eviction is a non-goal — both
//! `tenant_name` and `Claims.sub` are operator-provisioned tenant
//! identifiers (one key per tenant row); the keyspace cannot grow
//! unbounded under either design.
//!
//! **Disabled by default.** The limiter is constructed from an
//! `Option<RateLimitConfig>`: `None` → `check()` is a no-op that
//! always returns `Ok(())`. Operators enable via `gateway.toml
//! [rate_limit]` with explicit `per_minute` and `burst`. There is no
//! compiled-in default quota — 10/min was too low for CI
//! burst-submitting closures; the right value is workload-dependent.
//!
//! On rate-limit violation: the build handler sends `STDERR_ERROR`
//! with a wait-hint and returns early. The SSH connection stays open
//! — the client can retry after the hinted delay.
// r[impl gw.rate.per-tenant]

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::{Clock, QuantaClock, QuantaInstant};
use governor::{DefaultKeyedRateLimiter, NotUntil, Quota};

/// Key for builds from sessions with an empty `tenant_name`
/// (single-tenant mode — no comment on the authorized_keys line). All
/// anonymous sessions share one bucket.
const ANON_KEY: &str = "__anon__";

/// Operator-supplied quota. Both fields must be ≥1 (`governor::Quota`
/// takes `NonZeroU32`). `burst` is the bucket capacity; `per_minute`
/// is the refill rate. A burst of N means N builds can fire
/// back-to-back before the limiter kicks in — useful for
/// `nix copy --to ssh-ng://…` which uploads a closure worth of
/// derivations then submits one build per root.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
pub struct RateLimitConfig {
    pub per_minute: u32,
    pub burst: u32,
}

/// Per-tenant token-bucket limiter.
///
/// `Clone` is cheap (inner `Arc`). One limiter is held on
/// [`GatewayServer`](crate::server::GatewayServer), cloned into each
/// [`ConnectionHandler`](crate::server::ConnectionHandler), then into
/// every protocol session's
/// [`SessionContext`](crate::handler::SessionContext). All clones
/// share the same `dashmap` so a tenant's budget is shared across
/// their concurrent SSH connections.
#[derive(Clone)]
pub struct TenantLimiter {
    inner: Option<Arc<DefaultKeyedRateLimiter<String>>>,
    /// Held for [`NotUntil::wait_time_from`] — governor's clock is
    /// monotonic, not `std::time::Instant`. The limiter and the
    /// wait-hint computation must agree on the clock source.
    clock: QuantaClock,
}

impl TenantLimiter {
    /// `config: None` → disabled (every `check()` passes).
    ///
    /// Panics if `per_minute == 0` or `burst == 0` — both are
    /// operator misconfiguration, better to fail loud at startup than
    /// silently rate-limit to zero.
    pub fn new(config: Option<RateLimitConfig>) -> Self {
        let clock = QuantaClock::default();
        let inner = config.map(|c| {
            let per_minute =
                NonZeroU32::new(c.per_minute).expect("rate_limit.per_minute must be >= 1");
            let burst = NonZeroU32::new(c.burst).expect("rate_limit.burst must be >= 1");
            let quota = Quota::per_minute(per_minute).allow_burst(burst);
            Arc::new(DefaultKeyedRateLimiter::keyed(quota))
        });
        Self { inner, clock }
    }

    /// Disabled limiter (for tests / call sites that don't want rate
    /// limiting without constructing an `Option`).
    pub fn disabled() -> Self {
        Self::new(None)
    }

    /// Check and consume one token for `tenant`.
    ///
    /// `tenant` is `Option<&str>` to match
    /// [`SessionContext::tenant_name`](crate::handler::SessionContext)
    /// semantics: empty string or `None` both map to the anonymous
    /// bucket (`"__anon__"`).
    ///
    /// Returns `Err(wait)` when the bucket is empty; `wait` is how
    /// long until the next token is available. Caller surfaces this
    /// as a hint in `STDERR_ERROR`.
    pub fn check(&self, tenant: Option<&str>) -> Result<(), Duration> {
        let Some(limiter) = &self.inner else {
            return Ok(());
        };
        let key = match tenant {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => ANON_KEY.to_string(),
        };
        limiter
            .check_key(&key)
            .map_err(|nu: NotUntil<QuantaInstant>| {
                // Measured from the limiter's own clock — governor's
                // QuantaClock, not std::time::Instant. Mismatching the
                // clock here gives a bogus hint (and QuantaInstant
                // isn't directly comparable to Instant anyway).
                nu.wait_time_from(self.clock.now())
            })
    }
}

// r[verify gw.rate.per-tenant]
#[cfg(test)]
mod tests {
    use super::*;

    /// Disabled limiter is a no-op. 10_000 checks all pass.
    #[test]
    fn disabled_never_blocks() {
        let lim = TenantLimiter::disabled();
        for _ in 0..10_000 {
            assert!(lim.check(Some("tenant-a")).is_ok());
        }
    }

    /// With burst=30: exactly 30 back-to-back checks pass, the 31st
    /// blocks. The refill rate (per_minute=10) doesn't matter here
    /// because all 31 calls happen within microseconds — far less
    /// than one token's refill interval (6s).
    #[test]
    fn burst_then_block() {
        let lim = TenantLimiter::new(Some(RateLimitConfig {
            per_minute: 10,
            burst: 30,
        }));
        for i in 0..30 {
            assert!(
                lim.check(Some("tenant-a")).is_ok(),
                "check #{i} should pass (within burst)"
            );
        }
        let wait = lim
            .check(Some("tenant-a"))
            .expect_err("check #31 should be blocked");
        // Wait hint is bounded by one refill interval (60s/10 = 6s).
        // We don't assert an exact value — governor's GCRA math gives
        // the time-to-NEXT-token, which is refill-interval minus
        // elapsed-since-last-refill. With a freshly-drained bucket
        // and microsecond test runtime, it's close to but not
        // exactly 6s. Bounds: more than zero, at most one interval.
        assert!(
            wait > Duration::ZERO && wait <= Duration::from_secs(6),
            "wait hint out of bounds: {wait:?}"
        );
    }

    /// Keyspace isolation: exhausting tenant-a's bucket doesn't
    /// touch tenant-b's. This is the whole point of per-tenant
    /// limiting — one noisy tenant can't DoS another.
    #[test]
    fn tenants_independent() {
        let lim = TenantLimiter::new(Some(RateLimitConfig {
            per_minute: 10,
            burst: 30,
        }));
        // Drain A.
        for _ in 0..30 {
            lim.check(Some("tenant-a")).unwrap();
        }
        assert!(lim.check(Some("tenant-a")).is_err(), "A is drained");
        // B is untouched.
        for i in 0..30 {
            assert!(
                lim.check(Some("tenant-b")).is_ok(),
                "B check #{i} should pass (independent bucket)"
            );
        }
        // A is still drained after B's 30 checks (no cross-pollination).
        assert!(
            lim.check(Some("tenant-a")).is_err(),
            "A still drained; B's consumption must not refill A"
        );
    }

    /// None and empty-string both map to the anon bucket. Draining
    /// via `None` means `Some("")` is also drained — same key.
    #[test]
    fn anon_key_aliases() {
        let lim = TenantLimiter::new(Some(RateLimitConfig {
            per_minute: 10,
            burst: 3,
        }));
        lim.check(None).unwrap();
        lim.check(Some("")).unwrap();
        lim.check(None).unwrap();
        // 4th on the shared anon bucket → blocked.
        assert!(
            lim.check(Some("")).is_err(),
            "None and empty-string share the __anon__ bucket"
        );
    }

    /// Shared-state across clones: two clones of the same limiter
    /// drain the SAME bucket. This is what makes the
    /// per-connection-handler clone safe — it's not N independent
    /// limiters, it's N handles to one.
    #[test]
    fn clones_share_state() {
        let lim = TenantLimiter::new(Some(RateLimitConfig {
            per_minute: 10,
            burst: 2,
        }));
        let lim2 = lim.clone();
        lim.check(Some("x")).unwrap();
        lim2.check(Some("x")).unwrap();
        // Two clones consumed from one bucket → third check (either
        // clone) is blocked.
        assert!(lim.check(Some("x")).is_err());
        assert!(lim2.check(Some("x")).is_err());
    }
}
