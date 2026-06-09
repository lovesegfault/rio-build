//! Executor-kind derivation ([`kind_for_drv`]) and [`RetryPolicy`]
//! (backoff configuration).
//!
//! The stream-era `ExecutorState` (per-connection heartbeat/capacity
//! tracking) was deleted with the operator-surface re-point: pull-mode
//! executors hold no scheduler-side connection state — the durable
//! open-attempt row is the per-executor record.

use rio_proto::types::ExecutorKind;

/// FOD ⇔ Fetcher airgap boundary (ADR-019). Every site that derives
/// `ExecutorKind` from a derivation's `is_fixed_output` goes through
/// this so adding a third kind doesn't miss one.
// r[impl sched.dispatch.fod-to-fetcher+2]
pub fn kind_for_drv(is_fixed_output: bool) -> ExecutorKind {
    if is_fixed_output {
        ExecutorKind::Fetcher
    } else {
        ExecutorKind::Builder
    }
}

/// Retry policy configuration.
///
/// `#[serde(default)]` on the struct → absent keys fall through to
/// `Default::default()`, so `[retry] max_retries = 5` leaves the
/// backoff curve unchanged. `PartialEq` is for the TOML-roundtrip
/// tests (`assert_eq!(cfg.retry, RetryPolicy::default())`). Float
/// fields mean this is a BITWISE compare — acceptable for config
/// (the test just asserts default-constructed identity, not
/// computed-value equality).
// r[impl sched.retry.attempts-bounded+5]
// The budget caps. Every failure-driven retry loop is bounded by one
// of these (or by `PoisonConfig.threshold` / POISON_RESUBMIT_RETRY_
// LIMIT); the per-site charge/check discipline is the reference fold
// in retry_policy.rs.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RetryPolicy {
    /// Maximum number of retries for transient failures.
    pub max_retries: u32,
    /// Maximum number of retries for InfrastructureFailure. Higher
    /// than `max_retries` because infra failures are executor-local
    /// (not the build's fault) — but NOT unbounded: a scheduler-side
    /// bug that misclassifies a deterministic failure as infra
    /// (e.g., empty CA input path → MetadataFetch error) would
    /// otherwise hot-loop forever. Observed: 9748 re-dispatches in
    /// one session before the CA-path-propagation fix landed.
    pub max_infra_retries: u32,
    /// Maximum number of `TimedOut` re-dispatches before the
    /// derivation goes terminal (`Cancelled`). Each `TimedOut` retry
    /// bumps `resource_floor.deadline` (I-200, `r[sched.timeout.promote-
    /// on-exceed]`), so this caps how many floor doublings a build
    /// can climb on timeout before the operator gets a visible
    /// failure. Default 4 (16× the initial deadline); a build that
    /// times out at 16× is genuinely stuck. Separate counter from
    /// `max_retries` (timeouts don't
    /// eat the transient budget) and from `max_infra_retries` (no
    /// time-window reset — sparse timeouts over hours are still the
    /// same hung build).
    pub max_timeout_retries: u32,
    /// Seconds since the LAST infra failure after which the
    /// `infra_retry_count` is reset to 0. Infra failures are by
    /// definition transient — N quick failures suggests a
    /// misclassified permanent error, but N failures spread over
    /// hours are independent incidents and shouldn't accumulate
    /// toward poison. I-127: a leaked PutPath lock caused 4 builders
    /// in a row to hit "concurrent PutPath"; the drv was poisoned at
    /// 99.7% despite being fine. With a window, the counter resets
    /// once the cluster self-heals (lock released, store recovered).
    pub infra_retry_window_secs: f64,
    /// Maximum number of `exempt_from_cap` infra-retry attempts before
    /// the derivation is poisoned. CONCURRENT_PUTPATH and
    /// `floor_outcome.promoted` skip `infra_count++` entirely, so a
    /// leaked store-side placeholder lock (the I-125a class) makes
    /// every honest worker report the exempt message → infinite pod
    /// churn at `info!` level only. This high-water cap is the
    /// scheduler-side terminal: every other completion status has one
    /// (`max_retries`, `max_infra_retries`, `max_timeout_retries`,
    /// size-tier ladder); without this, the I-127 exemption was the
    /// sole worker-reportable status with zero scheduler-side bound.
    pub max_exempt_infra_retries: u32,
    /// Base backoff duration in seconds.
    pub backoff_base_secs: f64,
    /// Backoff multiplier.
    pub backoff_multiplier: f64,
    /// Maximum backoff duration in seconds.
    pub backoff_max_secs: f64,
    /// Jitter fraction (0.0 to 1.0).
    pub jitter_fraction: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            // InfrastructureFailure has NO backoff (re-dispatch is
            // immediate) so a misclassified permanent failure hot-loops.
            // Observed: 12 derivations cycled 146 times in 6 minutes
            // when an S3 auth failure was reported as infra. The cap
            // converts that into a visible poison.
            //
            // I-127 raised 5→10 + added the time-window reset below.
            // 5 was too tight under shallow-1024x: a leaked PutPath
            // lock made 4 builders in a row report infra → poison at
            // 99.7% on a perfectly buildable drv. 10 attempts (no
            // backoff, so still seconds-to-low-minutes for fast
            // builds) gives the cluster room to self-heal, and the
            // 5-min window means slow-drip failures don't accumulate.
            // A true misclassified permanent failure still poisons in
            // 10 immediate cycles — well under a minute.
            max_infra_retries: 10,
            // I-200: 4 = number of promotions to walk a 5-class
            // ladder (tiny→xlarge). After that, terminal Cancelled.
            // Each retry goes to a larger class with a 5× longer
            // activeDeadlineSeconds, so the wall-clock cost is
            // bounded by Σ(cutoff_i × 5) ≈ 5× the largest class
            // cutoff — not unbounded.
            max_timeout_retries: 4,
            // 5min: long enough that a stuck lock (I-125a's leaked
            // PutPath, ~tens of seconds to recover) or a store
            // restart doesn't compound across attempts; short enough
            // that the 9748-dispatch hot-loop scenario above
            // (146 cycles / 6min ≈ 2.5s/cycle) still hits the cap
            // before the window resets it.
            infra_retry_window_secs: 300.0,
            // I-127's observed benign ceiling is 4-in-a-row; 50 gives
            // >10× headroom while still terminating a leaked-lock
            // livelock within minutes (no backoff on the exempt path,
            // so 50 immediate cycles ≈ seconds-to-low-minutes for fast
            // builds). Separately bounds `floor_outcome.promoted` as
            // defense-in-depth against a `bump_resource_floor` bug
            // that always returns `promoted=true`.
            max_exempt_infra_retries: 50,
            backoff_base_secs: 5.0,
            backoff_multiplier: 2.0,
            backoff_max_secs: 300.0,
            jitter_fraction: 0.2,
        }
    }
}

impl RetryPolicy {
    /// Compute the backoff duration for a given retry attempt.
    ///
    /// Thin adapter over [`rio_common::backoff::Backoff`]: the serde
    /// field names (`backoff_base_secs` etc.) are the config-file
    /// contract, so this struct stays; the curve+NaN/overflow safety
    /// lives in the shared mechanism.
    pub fn backoff_duration(&self, attempt: u32) -> std::time::Duration {
        use rio_common::backoff::{Backoff, Jitter};
        Backoff {
            base: rio_common::clamped::clamped_duration_secs(self.backoff_base_secs),
            mult: self.backoff_multiplier,
            // merged_bug_262: a TOML `backoff_max_secs = "inf"`
            // degrades to the shared 1yr ceiling instead of crashing
            // config load (the clamp lives in the one constructor).
            cap: rio_common::clamped::clamped_duration_secs(self.backoff_max_secs),
            jitter: Jitter::Proportional(self.jitter_fraction),
        }
        .duration(attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_backoff() {
        let policy = RetryPolicy::default();
        let d0 = policy.backoff_duration(0);
        let d1 = policy.backoff_duration(1);

        // Base is 5s, so first attempt should be around 5s +/- jitter
        assert!(d0.as_secs_f64() > 3.0 && d0.as_secs_f64() < 7.0);
        // Second attempt should be around 10s +/- jitter
        assert!(d1.as_secs_f64() > 7.0 && d1.as_secs_f64() < 13.0);
    }

    /// Regression: backoff_max_secs = infinity (e.g., from a
    /// misconfigured TOML that had "inf" literally) must not panic
    /// in Duration::from_secs_f64. The 1-year clamp catches it.
    #[test]
    fn test_retry_backoff_infinity_clamped() {
        let policy = RetryPolicy {
            backoff_max_secs: f64::INFINITY,
            ..Default::default()
        };
        // Large attempt → base * multiplier^N → unbounded → .min(inf)
        // = inf → from_secs_f64(inf) would PANIC without the clamp.
        let d = policy.backoff_duration(100);
        // Clamped to 1yr. Jitter is applied BEFORE the clamp, and
        // inf * (1 +/- jitter) = inf, so the final value is exactly
        // 1yr (jitter has no effect on infinity).
        assert!(
            d.as_secs() <= 366 * 86400,
            "infinity backoff clamped to ~1yr"
        );
    }
}
