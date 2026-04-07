//! Graceful shutdown signal handling.
//!
//! Provides a [`CancellationToken`] that fires on SIGTERM/SIGINT, letting
//! `main()` return normally instead of the default SIGTERM behavior
//! (immediate termination, skipping atexit handlers).
//!
//! This matters for:
//! - LLVM coverage profraw flushing (written via `atexit`)
//! - tracing/OpenTelemetry span flush
//! - in-flight work drain
//!
//! Usage: call [`shutdown_signal()`] once at the top of `main()`, clone the
//! token for each background task, pass `.cancelled_owned()` to tonic's
//! `serve_with_shutdown` / axum's `with_graceful_shutdown`.
//!
//! `sighup_reload` is the hot-reload companion: spawns a SIGHUP
//! listener that invokes a closure on each signal. Used for JWT pubkey
//! rotation — the closure re-reads the ConfigMap mount and swaps the
//! `Arc<RwLock<VerifyingKey>>` that the interceptor reads.

use std::future::Future;

use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

// Re-export so downstream crates don't need to add tokio-util directly.
pub use tokio_util::sync::CancellationToken as Token;

/// Returns a token that is cancelled when SIGTERM or SIGINT is received.
///
/// Spawns a background task to listen for signals. Call once near the top
/// of `main()`. Clone the returned token for each background loop;
/// each loop should `select!` on `token.cancelled()` and break when it
/// fires. For tonic/axum servers, pass `token.cancelled_owned()` to
/// `serve_with_shutdown` / `with_graceful_shutdown`.
///
/// # Panics
///
/// Panics if signal handler installation fails (e.g., not in a Unix
/// environment). This is a programming error — signal handling is a
/// hard requirement for binaries using this helper.
pub fn shutdown_signal() -> CancellationToken {
    // Install handlers BEFORE spawning — otherwise there's a race where
    // a signal delivered between return and the spawned task's first poll
    // hits the default handler (terminate). signal() registers the
    // process-wide disposition synchronously; the Signal struct can be
    // moved into the spawned task for async .recv().
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, shutting down");
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received, shutting down");
            }
        }
        t.cancel();
    });
    token
}

/// Spawn a SIGHUP listener that runs `reload` on each signal.
///
/// The returned handle can be ignored (detached); the task runs for
/// the process lifetime. The `shutdown` token stops the loop — wire
/// it from [`shutdown_signal`] so SIGTERM cleanly exits the SIGHUP
/// listener too.
///
/// `reload` is async because the common pattern is: re-read a file
/// (ConfigMap mount), parse it, swap into `Arc<RwLock<T>>`. The file
/// read is `tokio::fs::read` (async); the swap is sync (std RwLock —
/// see jwt_interceptor.rs for why).
///
/// # SIGHUP is a loop, not a once
///
/// Unlike SIGTERM (one-shot: shutdown), SIGHUP fires repeatedly over
/// the process lifetime. Each rotation is one SIGHUP. The loop here
/// calls `.recv()` in a `while let` — tokio's `Signal` stream yields
/// once per signal delivery.
///
/// # Errors in `reload` are logged, not fatal
///
/// A failed reload (file missing, parse error) keeps the OLD key
/// active — the `Arc<RwLock>` swap simply doesn't happen. The
/// interceptor keeps verifying against whatever key was there before.
/// This is the safe degradation: a botched rotation doesn't brick the
/// cluster, it just means the old key is still required. Operator
/// sees the log, fixes the ConfigMap, SIGHUPs again.
///
/// # Example
///
/// ```ignore
/// let pubkey: Arc<RwLock<VerifyingKey>> = /* loaded at boot */;
/// let pk = Arc::clone(&pubkey);
/// let path = cfg.jwt.key_path.clone().unwrap();
/// sighup_reload(shutdown.clone(), move || {
///     let pk = Arc::clone(&pk);
///     let path = path.clone();
///     async move {
///         let bytes = tokio::fs::read(&path).await?;
///         let decoded = base64::decode(bytes.trim_ascii())?;
///         let arr: [u8; 32] = decoded.try_into()
///             .map_err(|_| anyhow::anyhow!("pubkey not 32 bytes"))?;
///         let new_key = VerifyingKey::from_bytes(&arr)?;
///         *pk.write().unwrap() = new_key;
///         Ok(())
///     }
/// });
/// ```
///
/// # Panics
///
/// Panics if SIGHUP handler installation fails — same as
/// [`shutdown_signal`], same rationale (Unix-only binaries, signal
/// handling is a hard requirement).
pub(crate) fn sighup_reload<F, Fut>(
    shutdown: CancellationToken,
    mut reload: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send,
{
    // Install BEFORE spawning — same race as shutdown_signal: a
    // SIGHUP delivered between return and the task's first poll
    // would hit the default handler (ignore for SIGHUP, but that
    // means the first rotation silently no-ops).
    let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!("SIGHUP reload loop: shutdown");
                    return;
                }
                got = sighup.recv() => {
                    // None = signal stream closed (shouldn't happen for
                    // SIGHUP while the process is alive, but handle it).
                    if got.is_none() {
                        tracing::warn!("SIGHUP signal stream closed");
                        return;
                    }
                    tracing::info!("SIGHUP received, reloading");
                    match reload().await {
                        Ok(()) => tracing::info!("SIGHUP reload succeeded"),
                        // Log + continue. The OLD state stays active.
                        // Operator sees this in logs, fixes the mount,
                        // SIGHUPs again.
                        Err(e) => tracing::warn!(error = %e, "SIGHUP reload failed; old state retained"),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the token transitions to cancelled after a self-delivered
    /// SIGTERM. Uses a bounded wait to avoid hanging if signal delivery
    /// is broken.
    #[tokio::test]
    async fn test_shutdown_on_sigterm() {
        let token = shutdown_signal();
        assert!(!token.is_cancelled(), "token should start uncancelled");

        // Raise SIGTERM to this process. tokio's signal handler will
        // catch it (installed above), so the default action (terminate)
        // is overridden.
        //
        // SAFETY: raise(3) is always safe — it just queues a signal
        // to the calling thread. No memory safety invariants.
        unsafe {
            libc::raise(libc::SIGTERM);
        }

        // Bound the wait; if this times out, signal delivery is broken.
        tokio::time::timeout(std::time::Duration::from_secs(5), token.cancelled())
            .await
            .expect("token should cancel within 5s of SIGTERM");

        assert!(token.is_cancelled());
    }

    /// SIGHUP fires the reload closure; shutdown token stops the loop.
    /// Verifies TWO things: (a) the closure runs on SIGHUP (reload is
    /// wired, not inert); (b) shutdown cleanly exits the loop (no
    /// dangling task on SIGTERM).
    #[tokio::test]
    async fn test_sighup_reload_fires_and_stops() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let shutdown = CancellationToken::new();

        let c = count.clone();
        let handle = sighup_reload(shutdown.clone(), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        });

        // Raise SIGHUP. Same pattern as test_shutdown_on_sigterm above.
        // SAFETY: raise(3) just queues a signal; no memory invariants.
        unsafe {
            libc::raise(libc::SIGHUP);
        }

        // Bound the wait. Poll the counter until it bumps or timeout.
        // No tokio::time::timeout on a bare AtomicU32 load — spin with
        // a short sleep instead (yields to let the spawned task run).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "reload closure should fire within 5s of SIGHUP"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(count.load(std::sync::atomic::Ordering::SeqCst) >= 1);

        // Shutdown stops the loop — handle completes.
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("SIGHUP loop should exit on shutdown within 5s")
            .expect("loop task should not panic");
    }

    /// Reload error → logged, loop continues. Next SIGHUP still fires.
    /// Proves "botched rotation doesn't brick the listener" — the loop
    /// isn't `?`-propagating the error out.
    #[tokio::test]
    async fn test_sighup_reload_error_keeps_looping() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let shutdown = CancellationToken::new();

        let a = attempts.clone();
        let _handle = sighup_reload(shutdown.clone(), move || {
            let a = a.clone();
            async move {
                a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Always fail — simulate a ConfigMap parse error.
                anyhow::bail!("simulated reload failure")
            }
        });

        // Two SIGHUPs. If the first error broke the loop, the second
        // wouldn't fire.
        // SAFETY: raise(3) is always safe.
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        // Yield to let the first reload run + fail + loop back.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            assert!(std::time::Instant::now() < deadline, "first SIGHUP timeout");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "second SIGHUP timeout — loop should survive first error"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Both attempts fired. Loop survived the first failure.
        assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        shutdown.cancel();
    }
}
