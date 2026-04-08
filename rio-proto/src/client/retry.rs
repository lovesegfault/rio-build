//! Shutdown-aware connect retry with exponential backoff.
//!
//! The cold-start connect loop used to be open-coded in every binary's
//! `main.rs` (gateway, controller, builder, scheduler — 4 copies), each
//! with the same two bugs:
//!
//! 1. **SIGTERM-blind**: `tokio::time::sleep(2s)` doesn't check the
//!    shutdown token. A SIGTERM arriving mid-sleep is ignored until
//!    the next connect attempt fails, adding up to 2s of grace-period
//!    waste per iteration. Under a tight `terminationGracePeriodSeconds`,
//!    that's enough to trip SIGKILL.
//! 2. **No backoff**: fixed 2s interval. When an upstream is genuinely
//!    down (not just cold-start racing), N replicas × 0.5 Hz connection
//!    storms the Service VIP for no benefit. Exponential backoff with
//!    jitter spreads the herd.
//!
//! [`connect_with_retry`] fixes both: each sleep races against
//! `shutdown.cancelled()`, and delays double (1s→2s→4s→8s→16s cap)
//! with ±25% jitter.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::warn;

use rio_common::backoff::{Backoff, Jitter};

/// Re-export so callers keep `rio_proto::client::retry::RetryError`.
pub use rio_common::backoff::RetryError;

/// Connect-retry curve. 1s base: under cold-start (the common case —
/// helm install, node drain+reschedule), dependencies are usually
/// ready within single-digit seconds; 1+2+4=7s covers that by attempt
/// 4. 16s cap: long enough to reduce connection-storm pressure on a
/// genuinely-down upstream, short enough that recovery is sub-minute
/// (worst case: upstream comes back just after a 16s sleep starts).
/// ±25% jitter so N replicas starting in lockstep (helm install)
/// don't synchronize their retry storms.
const CONNECT_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(16),
    jitter: Jitter::Proportional(0.25),
};

/// Retry `op` until it succeeds, `max_tries` is reached, or `shutdown`
/// fires.
///
/// # Backoff
///
/// Exponential: 1s, 2s, 4s, 8s, capped at 16s. Each delay gets ±25%
/// uniform jitter so N replicas starting in lockstep (helm install)
/// don't synchronize their retry storms.
///
/// # Shutdown
///
/// Every sleep is `select!`-raced against `shutdown.cancelled()`. If
/// the token fires during a sleep, returns [`RetryError::Cancelled`]
/// immediately — no final attempt, no extra delay. The token is also
/// checked before the first attempt, so a pre-cancelled token
/// short-circuits without ever calling `op`.
///
/// # `max_tries`
///
/// - `None`: retry forever. Returns `Ok(T)` or `Cancelled`, never
///   `Exhausted`. Use for binaries where "can't reach dependency" =
///   "useless process" (gateway without scheduler, builder without
///   store) — the pod stays not-Ready, kubelet doesn't restart it,
///   and it recovers the instant the dependency appears.
/// - `Some(n)`: give up after `n` failed attempts. Use when the
///   dependency is optional and the caller can degrade gracefully
///   (scheduler's store client: missing → CA-cutoff disabled, not
///   fatal).
pub async fn connect_with_retry<F, Fut, T, E>(
    shutdown: &CancellationToken,
    mut op: F,
    max_tries: Option<u32>,
) -> Result<T, RetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    // Thin wrapper over the shared `retry` loop: every error is
    // retryable (connect has no permanent-vs-transient distinction —
    // the caller picked `None` precisely because "can't reach
    // dependency" = "useless process"), and `None` maps to MAX
    // (effectively infinite — exit via `Ok` or `Cancelled`).
    //
    // `AtomicU32` for `tries`: both closures capture it (`is_retryable`
    // reads for the log line, `op` increments) so `&mut` would
    // conflict; and the returned future must be `Send` (callers
    // `tokio::spawn` it), which rules out `Cell`.
    use std::sync::atomic::{AtomicU32, Ordering};
    let tries = AtomicU32::new(0);
    rio_common::backoff::retry(
        &CONNECT_BACKOFF,
        max_tries.unwrap_or(u32::MAX),
        shutdown,
        |e: &E| {
            warn!(
                error = %e, tries = tries.load(Ordering::Relaxed),
                "connect failed; retrying (pod stays not-Ready)"
            );
            true
        },
        || {
            tries.fetch_add(1, Ordering::Relaxed);
            op()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Shutdown during sleep returns Cancelled promptly — the key
    /// correctness property. Without the select!, this test would
    /// block for the full backoff duration.
    #[tokio::test(start_paused = true)]
    async fn cancelled_during_sleep_returns_promptly() {
        let token = CancellationToken::new();
        let calls = Arc::new(AtomicU32::new(0));

        let calls2 = Arc::clone(&calls);
        let token2 = token.clone();
        let task = tokio::spawn(async move {
            connect_with_retry(
                &token2,
                || {
                    let c = Arc::clone(&calls2);
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Err::<(), _>("nope")
                    }
                },
                None,
            )
            .await
        });

        // Let the first attempt fail and enter its backoff sleep.
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "first attempt should fire");

        // Cancel mid-sleep. The task must resolve without advancing
        // time to the sleep deadline — proving select! woke on the
        // token, not the timer.
        token.cancel();
        tokio::task::yield_now().await;

        let result = task.await.unwrap();
        assert!(matches!(result, Err(RetryError::Cancelled)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no retry after cancellation"
        );
    }

    /// Pre-cancelled token short-circuits before the first attempt.
    #[tokio::test]
    async fn pre_cancelled_never_calls_op() {
        let token = CancellationToken::new();
        token.cancel();
        let calls = AtomicU32::new(0);

        let result: Result<(), _> = connect_with_retry(
            &token,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("unreachable") }
            },
            None,
        )
        .await;

        assert!(matches!(result, Err(RetryError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Bounded retry: exhausts after N tries, returns last error.
    #[tokio::test(start_paused = true)]
    async fn bounded_exhausts_after_max_tries() {
        let token = CancellationToken::new();
        let calls = AtomicU32::new(0);

        let result: Result<(), _> = connect_with_retry(
            &token,
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Err(format!("attempt {n}")) }
            },
            Some(3),
        )
        .await;

        match result {
            Err(RetryError::Exhausted { last, attempts }) => {
                assert_eq!(attempts, 3);
                assert_eq!(last, "attempt 3");
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Success on first try returns immediately, no sleep.
    #[tokio::test]
    async fn immediate_success() {
        let token = CancellationToken::new();
        let result = connect_with_retry(&token, || async { Ok::<_, &str>(42) }, None).await;
        assert_eq!(result.unwrap(), 42);
    }

    /// Success after N failures: returns Ok, stops retrying.
    #[tokio::test(start_paused = true)]
    async fn eventual_success() {
        let token = CancellationToken::new();
        let calls = Arc::new(AtomicU32::new(0));

        let calls2 = Arc::clone(&calls);
        let result = connect_with_retry(
            &token,
            move || {
                let c = Arc::clone(&calls2);
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 { Err("not yet") } else { Ok(n) }
                }
            },
            None,
        )
        .await;

        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Jitter stays within ±25% bounds.
    #[test]
    fn jitter_bounds() {
        let base = Duration::from_secs(4);
        for _ in 0..1000 {
            let j = CONNECT_BACKOFF.jitter.apply(base);
            assert!(j >= Duration::from_secs(3), "{j:?} below 75% of {base:?}");
            assert!(j <= Duration::from_secs(5), "{j:?} above 125% of {base:?}");
        }
    }
}
