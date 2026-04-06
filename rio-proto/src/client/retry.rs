//! Shutdown-aware connect retry with exponential backoff.
//!
//! The cold-start connect loop used to be open-coded in every binary's
//! `main.rs` (gateway, controller, worker, scheduler — 4 copies), each
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
use tracing::{info, warn};

/// Terminal state of [`connect_with_retry`].
#[derive(Debug)]
pub enum RetryError<E> {
    /// Shutdown token cancelled while waiting between attempts. The
    /// caller's `main()` should treat this as a clean exit — the
    /// process is being terminated, there's nothing to connect to.
    Cancelled,
    /// `max_tries` reached without success. `last` is the final
    /// attempt's error. Only reachable when `max_tries` is `Some`;
    /// infinite retries exit via `Cancelled` or `Ok`.
    Exhausted { last: E, tries: u32 },
}

impl<E: std::fmt::Display> std::fmt::Display for RetryError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryError::Cancelled => write!(f, "connect retry cancelled by shutdown"),
            RetryError::Exhausted { last, tries } => {
                write!(f, "connect failed after {tries} tries: {last}")
            }
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for RetryError<E> {}

/// Backoff ceiling. 16s: long enough to reduce connection-storm
/// pressure on a genuinely-down upstream, short enough that recovery
/// after upstream comes back is still sub-minute (worst case: upstream
/// recovers just after a 16s sleep starts → ~16s extra latency).
const BACKOFF_CAP: Duration = Duration::from_secs(16);

/// Initial backoff. 1s: under cold-start (the common case — helm
/// install, node drain+reschedule), dependencies are usually ready
/// within single-digit seconds. Starting at 1s means the first few
/// retries cover that window tightly (1+2+4=7s by attempt 4).
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);

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
///   "useless process" (gateway without scheduler, worker without
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
    let mut delay = BACKOFF_INITIAL;
    let mut tries = 0u32;

    loop {
        // Pre-check: if shutdown already fired (e.g., SIGTERM raced
        // process startup and won), don't bother attempting. Also
        // catches the case where the previous iteration's select!
        // resolved on sleep-complete at the same instant the token
        // fired — next iteration sees it here.
        if shutdown.is_cancelled() {
            info!("connect retry cancelled by shutdown");
            return Err(RetryError::Cancelled);
        }

        tries += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if max_tries.is_some_and(|max| tries >= max) {
                    return Err(RetryError::Exhausted { last: e, tries });
                }
                let jittered = jitter(delay);
                warn!(
                    error = %e, tries, backoff = ?jittered,
                    "connect failed; retrying (pod stays not-Ready)"
                );
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {
                        info!("connect retry cancelled by shutdown");
                        return Err(RetryError::Cancelled);
                    }
                    () = tokio::time::sleep(jittered) => {}
                }
                // Double, cap at BACKOFF_CAP. saturating_mul: delay
                // is at most 16s so 2× never overflows, but explicit
                // about intent.
                delay = delay.saturating_mul(2).min(BACKOFF_CAP);
            }
        }
    }
}

/// Apply ±25% uniform jitter. `d` of 4s → somewhere in [3s, 5s].
///
/// Jitter prevents thundering-herd synchronization: N gateway pods
/// started by the same `helm install` all hit their first connect
/// failure at ~the same instant. Without jitter, they'd all retry
/// at t+1s, t+3s, t+7s… in lockstep. ±25% spreads each wave across
/// a half-second-to-several-second window.
fn jitter(d: Duration) -> Duration {
    // rand::random::<f64>() is [0, 1); map to [0.75, 1.25).
    let factor = 0.75 + rand::random::<f64>() * 0.5;
    d.mul_f64(factor)
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
            Err(RetryError::Exhausted { last, tries }) => {
                assert_eq!(tries, 3);
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
            let j = jitter(base);
            assert!(j >= Duration::from_secs(3), "{j:?} below 75% of {base:?}");
            assert!(j <= Duration::from_secs(5), "{j:?} above 125% of {base:?}");
        }
    }
}
